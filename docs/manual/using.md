<!-- made by CasualArclamp -->
# DecSAT manual: The window, Identify and Survey

This page tells you how to use the DecSAT window and its general tools. [Back to the README](../../README.md).

## The window

Start DecSAT with a capture:

```bash
cargo run --release -p decsat-gui -- capture_8Msps.cs8
```

You can also open a capture with **File ▸ Open IQ…**, or drop the file on
the window. DecSAT reads the sample rate, the centre frequency and the
format from the file name when the name contains them. The recordings of
gqrx, SDR++ and DecSAT have such names. For other files, set these values
under *Raw IQ files* in the File menu. Then push **⟳ Apply**.

DecSAT gives each recording a name that contains its RF frequency, e.g.
`decsat-ASA245_LHI-BN-12346.0340MHz-…`. This applies to recordings of IQ,
symbols, modem data, PCAP, TS and voice. The RF frequency is the frequency
of the radio plus the LNB LO and the LO trim. When you open an IQ recording
again, DecSAT takes the centre frequency from the name.

Sometimes the RF is not known (no LO is set, or a file has no centre
frequency). Then the name contains the offset of the VFO from the centre of
the band (`+5236688Hz`). Recordings from versions before 0.8.1 also have
such names.

On the waterfall:

| | |
|---|---|
| **wheel** | Zoom at the cursor. When the zoom is more than one FFT bin a pixel, DecSAT transforms the part on the screen again at a finer resolution. The limit is 1 Hz bins on a 500 Hz view |
| **right- or middle-drag** | Pan. With a live HackRF, a pan past the edge of the span (or at full span) tunes the radio. The spectrum follows the mouse. The VFOs stay on their carriers and continue to decode. A VFO that goes out of the span stops until **Tune to it** in its settings brings it back |
| **drag on empty space** | Draw a new VFO |
| **double-click** | Put a VFO there |
| **click a green bracket** | Claim a detected carrier with a VFO of the correct width. The VFO uses the decoder that DecSAT knows the carrier needs. If DecSAT does not know, it finds the decoder as the Survey does (Identify, then DVB-S2 or the Comtech *type found*) |
| **shift-click a green bracket** | Claim the carrier with the decoder that *New VFOs run* gives |
| **click a violet dashed bracket** | Make a TDMA VFO across a band whose power comes and goes. A grey dashed bracket is Bluetooth that leaks into the HackRF, not the satellite |
| **drag a VFO** / **its edge** | Move the VFO / change its width |
| **click** (VFO selected) | Tune the VFO to that frequency |
| **✕** on a VFO's label, or **Delete** | Remove the VFO |

The large readout at the top is the centre frequency, as in SDR++:

- Turn the wheel above a digit to change it by one step.
- Click the upper half or the lower half of a digit to change it by one
  step.
- Right-click a digit to set it and all the digits below it to zero.

If the HackRF operates, a change of the readout tunes the radio.

**File ▸ 📁 Output folder…** sets the folder for IQ recordings, symbol
files, PCAP files and TS files. The default is `Documents\DecSAT`. DecSAT
keeps the setting between runs. The setting also applies to the VFOs that
are there now.

The side bar lists the VFOs with their CPU load (✕ removes one). For the
selected VFO, the side bar shows:

- its settings
- its Identify result or its demodulator state (lock, Es/N0 and Eb/N0,
  residual offset)
- a constellation that is locked to the carrier
- a spectrum with zoom.

A DVB-S, S2 or S2X VFO opens with a card in the colour of the standard (S2X
blue, S2 green, DVB-S yellow). The card shows:

- in its title bar, the standard, CCM or ACM, and the symbol rate
- in large text, the MODCOD that the VFO receives, with an Es/N0 bar and
  the margin above the quasi-error-free Es/N0 of the tables in EN 302 307
- each stage of the receive chain, with a health dot:
  - the demodulator
  - the PL frames
  - LDPC and BCH (for an ACM carrier, its MODCODs with their share, good
    frames and margin)
  - the load of the FEC thread
  - the super-frames, when the carrier has them
  - the baseband.

For a multistream carrier, the input streams are below the baseband. Each
stream shows the data that it contains, its MODCOD and its rate. Click a
stream that has transport packets to show its TS in the card below.

The status bar shows the number of carriers and narrow lines, and the load
of the front end. The input level is adjacent to the play button. The input
level is orange when the gain of the HackRF is too high (see
[HackRF One](radios.md)).

## A radio on another machine

`decsat-server` runs on the computer that has the radio. DecSAT connects to
it from any location (Source ▸ Remote server settings…). There are two
modes:

- **Smart** mode decodes on the server. It sends only the waterfall, the
  constellations, the decoder cards and the TS. The sound of each decoder
  (Opus), the files that the decoder writes and its Wireshark feeds come
  back to your machine. Smart mode runs every decoder.
- **Dumb** mode sends the IQ. It uses 4 to 16 bits with block scaling and
  optional zstd. It sends the whole band or a slice.

`--shared` runs one radio for all users, as on QO-100. Each user adds
decoders and can also use the decoders of other users. For the details, see
[docs/REMOTE.md](../REMOTE.md).

```bash
decsat-server --bind 0.0.0.0:5262 --password-file pw.txt
decsat-server --bind 0.0.0.0:5262 --shared hackrf --center 739.5e6 --rate 8e6
```

`decsat-server-app.exe` is the same server with a window. Double-click it.
Select who can connect. Push Start.

The link is not encrypted. On the internet, use a VPN or an SSH tunnel.

## Survey

**Tools ▸ Survey…** tries every clean carrier that the receiver sees. It
goes in frequency order and tries 1–4 carriers at the same time (*At
once*). For each carrier, the survey does these steps:

1. It gives the carrier its own VFO for Identify.
2. It starts the decoder for the signal type. A DVB-S2 carrier gets the
   GSE/IP decoder. A modem carrier (TPC 2964, the K = 7 code, a FastLink)
   gets the *type found* model of the Comtech entry. Bursts, and signals
   that Identify cannot name, have no decoder at this time.
3. It gives the decoder a time to lock (*give up after*, 40 s by default).
4. If the decoder locks, the survey monitors it for a few more seconds.
   Thus the data that the carrier contains shows.
5. It removes the VFO and goes to the next carrier.

During a survey, DecSAT does not play or record.

The window lists these items for each carrier:

- the frequency, the S/N and the rate
- the result (decoded, no lock, no decoder, no signal)
- the signal type that the decoder found
- the data that the carrier contains (ADS-B, radar, voice)
- the payload, or the last message of the decoder.

**VFO** puts a permanent VFO on a decoded carrier. **Save CSV** writes the
table to the output folder. The ADS-B and RADAR tags of the waterfall get
their data during the survey. A retune stops the survey. `--survey` runs a
survey unattended (add `--screenshot` to keep the picture).

**Reset labels** is in the Satellite menu and in the survey window. One
click removes every carrier label on the waterfall and the results of the
survey. The labels are the modem, ADS-B and RADAR tags, which DecSAT keeps
between runs. Use it after false decodes, or after the carriers of the
satellite moved. A carrier that a VFO continues to decode gets its label
again.

A click on the green bracket of one carrier does the same steps for that
carrier only, and keeps the VFO. The VFO starts on Identify. Then it
changes to the decoder for the signal that Identify saw, at the centre and
rate that Identify measured. There are two special cases:

- A DVB-S2 carrier whose frames are TS-mode goes on to the MPEG-TS decoder.
- A carrier that has no decoder in DecSAT stays on Identify.

The status bar shows the decoder that DecSAT selected. A carrier that was
decoded before (by a click or by a survey) gets that decoder immediately.
A tool that you selected in *New VFOs run* (IQ recorder, spectrum, symbols,
carrier ID) stays. A shift-click gives the carrier the decoder that *New
VFOs run* gives. `--claim-auto` claims every carrier this way unattended.

## Frequency plan

**Satellite ▸ 📋 Plan…** loads a frequency plan. A plan is an SDR++
frequency-manager bookmark list (`.json`: name, RF frequency and bandwidth
for each carrier). DecSAT uses the plan as follows:

- It marks the carriers of the plan along the bottom of the spectrum.
- It shows the names where there is space (zoom in to see more).
- When the pointer is above a carrier, DecSAT shows its name.
- A click on a name makes a VFO on that carrier with its name and width.
- A VFO that you draw or claim on a plan carrier gets the name of the
  carrier.

A carrier with `"color": "#rrggbb"` in the file gets a mark in that colour.
For example, you can use colours to show the modem types. SDR++ ignores the
field. The plan stays your own file: DecSAT keeps only its path. **Put the
plan away** stops the use of the plan.

**Satellites:** the plan and the carrier labels that the decoders make
belong to the satellite that the dish points to. The carriers of a
different satellite at the same frequencies are different carriers. The
satellite list at the top right (adjacent to **LO trim**) shows the
selected satellite. Each satellite keeps its own plan and labels. DecSAT
shows only the plan and labels of the selected satellite. It gives only
these to new VFOs, and **Reset labels** removes only these.

To add a satellite, type a name in the list and push **Add**. The new
satellite starts with no plan and no labels. **Rename this one** gives a
name to the selected satellite. The list shows a plan and labels from
before satellites had names as *(unnamed)* until you give them a name.

The oscillator of an LNB is rarely exactly on 10600 MHz, and it drifts as
its temperature increases. Thus the carriers are a few kHz away from their
plan frequencies. These controls correct the RF scale:

- **LO trim** at the top right corrects the RF scale immediately. It does
  not tune the radio or the VFOs. Drag it or type a value in kHz.
  Right-click to set it to zero.
- **Satellite ▸ ⟲ Align LO trim to the plan** sets the trim from the
  carriers in view. It searches ±3 MHz. It uses the trim at which most of
  the carriers are on plan carriers of approximately the same width.
- **keep it aligned** does the alignment again every 5 s in small steps as
  the LNB drifts.

The frequency bar shows the trim. DecSAT saves the trim with the session.

## Text in every decoder

Every decoder shows the text in the data that it decodes:

- **DVB-S2/S2X and DVB-S → MPEG-TS**: the payloads of the transport stream.
  The decoder reads each PID separately, so the strings of a table stay
  complete. Examples are service and provider names, EPG text, URLs and all
  other data that is not encrypted. When the stream contains IP (MPE), the
  decoder also reads the IP payloads.
- **DVB-S2/S2X → GSE/IP** and **TPC 2964 over HDLC**: the UDP/TCP payloads.
  Examples are SAP/SDP announcements, HTTP headers and plain-text
  protocols.
- **TPC 2964** with a payload that the decoder does not know, **Identify**
  while it demodulates live, and the **generic PSK** decoder: every reading
  of the bits, as above.

Decoded data can be compressed data, encrypted data or video. Such data
contains short printable strings by chance. Thus the byte decoders give a
rank to each string by how frequently it occurs again. Strings that occur
by chance do not occur again. The byte decoders also list long strings as
they come.

## What Identify reports

Identify keeps the things that it *knows* apart from the things that it
*guesses*.

**DVB-S2** is a fact. Identify reports it when the PLHEADERs repeat exactly
where their own PLS codes predict the next frame. The report includes the
MODCODs in use, the pilots, the frame lengths, and CCM or ACM. A MODCOD
that Identify reads one time in many frames is a header with an error.
Identify says so, and does not report ACM or S2X because of it.

All other signals get measurements (symbol rate, roll-off, an estimated
constellation) and a guess with a label:

- Plain QPSK shows as *"possibly DVB-S (not verified)"*. The DVB-S decoder
  confirms it.
- Identify names a BPSK, QPSK or 8PSK carrier as TPC 2964 if it has the
  TPC 2964 unique word every 2964 bits.
- Identify names such a carrier as Comtech TPC 7/8 if it has the unique
  word of Comtech TPC 7/8 every 16 320 bits.

Identify also names the signals of the **Intelsat 19 DAMA network**:

- **An outbound**: by its 176-symbol sync word every 40 112 symbols.
  Identify looks for the sync word differentially. Thus Identify finds it
  when the pilots make the carrier look like BPSK. Identify then reports
  QPSK. A claim of the carrier, or the survey, runs the IS-19 outbound
  decoder.
- **A call channel**: by its pilots, one every 16 symbols in 154-symbol
  frames. The report is *"Intelsat 19 DAMA call channel … its voice is
  encrypted"*. There is nothing to decode. The outbounds name its calls.
- **A wide call channel** (25.6 or 51.2 kBd, in duplex pairs as the voice
  channels are): by a pilot strictly every 16 symbols.

Call channels are 6.16 kHz apart, so carrier detection sees the band as one
carrier. To get the name of one channel, put a VFO (about 6.5 kHz wide) on
it.

For the other signals, Identify looks for fingerprints of proprietary
waveforms:

- **DVB-S2-style headers off their own grid**: SOF and PLS headers that
  occur regularly, but not where their PLS codes say the next frame starts.
  This is S2 framing whose codes have a different meaning. NovelSat NS3/NS4
  appear to use such framing. Their manuals describe S2-style headers, S2
  frame sizes and Gold-code scrambling, with extra code rates. The short
  frames of a vendor can also have such headers.
- **A 2 % roll-off**, which is sharper than the 5 % of DVB-S2X. This is the
  roll-off of NovelSat NS4.
- **A convolutional code** on BPSK or QPSK: with a rate-½ code, the coded
  bits satisfy a parity check that is K steps long. The taps of the check
  are the generators of the code. Identify finds the check in the hard
  decisions and names the code (*"QPSK, convolutional code K = 7, rate 1/2
  (171, 133)"*, or any other code up to K = 10). The type-found modem and
  the survey then run the Viterbi decoder, which decodes it.
- **Frame structure**: symbols that repeat in each frame (a header, a
  unique word or pilots) show in the autocorrelation of the locked symbols.
  Identify reports the period. With sufficient frames, it also reports the
  positions of the symbols that repeat (*"repeats every 3330 symbols; 162
  known symbols: 90 from 0, then 2 blocks of 36 every 1476"*). This is a
  fingerprint of a framing, also when no specification is public.

Comtech VersaFEC and the TPC/LDPC modes of SCPC modems have no published
sync word or frame layout. Thus only such measurements can identify them.
Captures of these signals can let DecSAT name them. The Comtech TPC 7/8
decoder came from such a capture: its idle frames repeated fully, and that
showed the structure.

The Paradise FastLink decoder also came from a capture, of a Q-Flex (QPSK
0.710). Identify names the signal, and the **Q-Flex FastLink** decoder
decodes it. The decoder also decodes the calls in the TDM multiplex. A call
is G.728 voice in four channels, two bits in each TDM word. The decoder
finds the position of the bits of each channel from the silence fill of the
voice equipment (a known pattern), while the channels are idle. The **Call
voice** card then shows when a person speaks. ▶ plays the call and ●
records it.

With **Voice: play when G.728 speech is found** (on by default), a call
plays automatically after a person spoke. The call and the Comtech channels
take turns, as **one voice at a time** sets. ⏹ sets this function to off
for the VFO.

The faster Q-Flex links (FastLink at 2, 3 or 4 × the rate of the 95 kBd
links) contain 2–4 × 128.5 kbit/s in the same Paradise framing. They have
the same 257-bit multiplex at that multiple of the frame rate, with a
different layout. At 3 ×, the multiplex is in superframes of three frames,
and the alignment column gives their position. DecSAT does these steps:

1. It finds the frames of the multiplex and measures its activity.
2. It finds a 64 kbit/s HDLC timeslot in the multiplex. This timeslot is
   bridged Ethernet on an inverted line: the IP network of the site. Its
   packets go to the IP view and the PCAP, as all other IP does.
3. It finds the calls of the radios in a different 64 kbit/s timeslot
   adjacent to the first one. The calls are G.728 on two bits of each
   octet. DecSAT finds them by their silence fill.

You hear the call in which a person speaks. The controls are the same as
for the slower links: ▶, ● and *play when G.728 speech is found*. For a
data recording, use `decsat payload … --voice-out call.wav`.

A carrier that is too slow to show three frames in the first look gets the
mark *provisional* while Identify listens longer. Between identifications,
Identify continues to demodulate with the values that it found. Thus the
constellation and the Es/N0 stay live.

## Snapshots, the session log and PCAP parts

**📷** adjacent to the input level (or **F12**) saves the spectrum and the
waterfall as a PNG in the output folder. A right-click (or **Shift+F12**)
saves the whole window. The file name contains the satellite, the RF and
the UTC time (`decsat-<satellite>-<RF>MHz-<UTC>-band.png`). The picture has
a caption with these items:

- the time
- the receiver and the rate
- the RF centre and the span
- the frequency of the radio and the LNB LO
- the satellite and the VFOs.

**File ▸ 📝 Session log** writes the reports of the VFOs to
`decsat-log-<UTC>.csv` in the output folder. Each line has the UTC time,
the VFO, the RF, the decoder, the kind and the text. The log contains:

- the results of Identify
- the decoder messages
- lock and loss of lock (DecSAT waits 2 s, so a lock that is not stable
  makes one line)
- each MODCOD when DecSAT sees it for the first time
- text found in the bits (strings that are readable text, 30 a minute
  maximum)
- a summary each minute
- VFOs that you add or change
- snapshots
- the notes of the status bar.

DecSAT writes a line when the statement changes. A change of only the
numbers is not a change of the statement. DecSAT writes the same statement
again one time a minute at most. Thus the file stays readable. A 📝
adjacent to the 📷 shows that the log is on. DecSAT keeps this setting.

**File ▸ PCAP parts** starts a new PCAP every 1–60 minutes during a PCAP
recording (`…-part002.pcap` and on). Each closed part is a complete
capture. Thus tools that read completed files (BruteShark, NetworkMiner)
can read the parts while the recording continues. Live Wireshark gets its
data through a pipe to its standard input, not through a network port. Thus
other tools cannot connect to that feed. `decsat decode … --pcap-rotate
MIN` does the same unattended.

## Which satellite

The NIT of a DVB network gives the orbital position of its transponders. A
DVB-RCS map gives the orbital position of its forward link. DecSAT finds
the name of the satellite at that position in a table of 78 positions
(177°W–180°E, `apps/decsat-gui/src/orbital_positions.json`). DecSAT shows
the name as follows:

- The network tab of the TS analyser and the RCS card show `28.2°E · Astra
  2E/2F/2G`.
- The toolbar shows the name in amber adjacent to the satellite list while
  a different satellite is selected.
- The satellite list shows it first. One click selects (or adds) "Astra
  2E/2F/2G (28.2°E)". The plan and the carrier labels of that satellite
  then apply.

The session log records each position that DecSAT finds.
