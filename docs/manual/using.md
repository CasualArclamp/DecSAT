<!-- made by CasualArclamp -->
# DecSAT manual: The window, Identify and Survey

This page tells you how to use the DecSAT window and its general tools. [Back to the README](../../README.md).

```bash
cargo run --release -p decsat-gui -- capture_8Msps.cs8
```

Or open a capture with **File ▸ Open IQ…**, or drop it on the window. The sample rate,
centre frequency and format are read from the file name when it carries them
(gqrx, SDR++ and DecSAT's own recordings do); otherwise set them under
*Raw IQ files* in the File menu and press **⟳ Apply**.

DecSAT names its recordings — IQ, symbols, modem data, PCAP, TS, voice — by
the RF frequency they hold, e.g. `decsat-ASA245_LHI-BN-12346.0340MHz-…`: the
radio's tuning plus the LNB LO and LO trim. Reopened, an IQ recording takes
its centre from that. When the RF is not known (no LO set, a file with no
centre) the name holds the VFO's offset from the band's centre instead
(`+5236688Hz`), as recordings from before 0.8.1 do.

On the waterfall:

| | |
|---|---|
| **wheel** | zoom about the cursor; past one FFT bin a pixel the part on screen is transformed again at a finer resolution, down to 1 Hz bins on a 500 Hz view |
| **right- or middle-drag** | pan; past the span's edge (or at full span) with the HackRF live, it tunes the radio — the spectrum follows the mouse, and the VFOs stay on their carriers, decoding on (one retuned out of the span idles until **Tune to it** in its settings brings it back) |
| **drag on empty space** | draw a new VFO |
| **double-click** | drop a VFO |
| **click a green bracket** | claim a detected carrier with a VFO sized to fit, on the decoder it is known to need, else found as the Survey finds it (Identify, then DVB-S2 or the Comtech *type found*) |
| **shift-click a green bracket** | claim it with the decoder *New VFOs run* instead |
| **click a violet dashed bracket** | a TDMA VFO over a band whose power comes and goes (grey dashed: Bluetooth leaking into the HackRF, not the satellite) |
| **drag a VFO** / **its edge** | move it / resize it |
| **click** (VFO selected) | tune it there |
| **✕** on a VFO's label, or **Delete** | remove it |

The big readout at the top is the centre frequency, as in SDR++: wheel over a
digit or click its upper/lower half to step it, right-click to zero that digit
and every one below it. With the HackRF running it retunes the radio.

**File ▸ 📁 Output folder…** sets where IQ recordings, symbol files,
PCAP and TS files go (default `Documents\DecSAT`); it is remembered between
runs and applies to existing VFOs too.

The side bar lists the VFOs with their CPU load (✕ removes one), and shows the
selected one's settings, its Identify result or demodulator state (lock, Es/N0 and Eb/N0,
residual offset), a carrier-locked constellation and a zoomed spectrum. A
DVB-S, S2 or S2X VFO opens with a card in the standard's colour (S2X blue,
S2 green, DVB-S yellow): the standard, CCM or ACM and the symbol rate in
its title bar; the MODCOD being received, large, with its margin over the
quasi-error-free Es/N0 of EN 302 307's tables and an Es/N0 bar; then the
receive chain stage by stage with health dots — demodulator, PL frames,
LDPC and BCH (an ACM carrier's MODCODs with their share, frames good and
margin), the FEC thread's load, the super-frames when the carrier has
them, the baseband — a multistream carrier's input streams under it, each
with what it carries, its MODCOD and rate (click one with transport
packets to show its TS in the card below). The
status bar counts the carriers and narrow lines and shows the front end's
load; the input level is beside the play button — orange when the HackRF's
gain is too high (see [HackRF One](radios.md)).

## A radio on another machine

`decsat-server` runs next to the radio; DecSAT connects from anywhere (Source
▸ Remote server settings…). **Smart** mode decodes on the server and sends
only the waterfall, constellations, decoder cards and the TS, with each
decoder's sound (Opus), the files it writes and its Wireshark feeds coming
back to your machine; **dumb** mode sends the IQ, 4 to 16 bits with block
scaling and optional zstd, whole band or a slice. Smart mode runs every
decoder.
`--shared` runs one radio for everyone, QO-100 style: each user adds decoders
and can camp on anyone else's. Details: [docs/REMOTE.md](../REMOTE.md).

```bash
decsat-server --bind 0.0.0.0:5262 --password-file pw.txt
decsat-server --bind 0.0.0.0:5262 --shared hackrf --center 739.5e6 --rate 8e6
```

Prefer a window? `decsat-server-app.exe` is the same server: double-click, choose who can connect, press Start.

The link is not encrypted: over the internet, use a VPN or SSH tunnel.

## Survey

**Tools ▸ Survey…** tries every clean carrier the receiver sees, in
frequency order, a few at a time (*At once*, 1–4): each gets a VFO of its
own for Identify, then the decoder for what it is — DVB-S2 to GSE/IP, the
modem carriers through the Comtech entry's *type found* (TPC 2964, the
K = 7 code, a FastLink); bursts, and anything Identify cannot name, have no decoder yet — and
a time to lock (*give up after*, 40 s by default). One that locks is
watched a few seconds more so what it carries shows, then its VFO goes and
the next carrier takes its turn; nothing plays or records meanwhile. The
window lists each carrier's frequency, S/N and rate, the outcome (decoded,
no lock, no decoder, no signal), what it decoded as, what it carries
(ADS-B, radar, voice) and its payload or the decoder's last word; **VFO**
puts a kept VFO on a decoded one, **Save CSV** writes the table to the
output folder. The waterfall's ADS-B and RADAR tags learn as it goes, and a
retune stops it. `--survey` runs one unattended (with `--screenshot` to
keep the picture). **Reset labels** (in the Satellite menu, and in the
survey's window) removes every carrier label on the waterfall — the modem,
ADS-B and RADAR tags, kept between runs — and the survey's results, in one
click: after false decodes, or once the satellite's carriers have moved. A
carrier with a VFO still decoding on it is labelled again.

A click on one carrier's green bracket does the same for that carrier
alone, and keeps the VFO: it starts on Identify and switches to the
decoder for what Identify saw, on the centre and rate it measured (a
DVB-S2 carrier whose frames turn out to be TS-mode moves on to the
MPEG-TS decoder; one with no decoder here stays on Identify). The status
bar says what it picked. A carrier already decoded once (here or by a
survey) gets that decoder straight away, and a tool chosen in *New VFOs
run* (IQ recorder, spectrum, symbols, carrier ID) is kept. Shift-click gives the carrier the decoder *New
VFOs run* instead; `--claim-auto` claims every carrier this way
unattended.

## Frequency plan

**Satellite ▸ 📋 Plan…** loads a frequency plan — an SDR++ frequency-manager bookmark
list (`.json`: name, RF frequency, bandwidth per carrier). Its carriers are
marked along the foot of the spectrum and named where there is room (zoom in
for more); hovering names the one under the pointer, clicking a name makes a
VFO on that carrier with its name and width, and a VFO drawn or claimed on a
plan carrier takes its name. A carrier given `"color": "#rrggbb"` in the
file is marked in that colour (to tell modem types apart, say); SDR++ ignores
the field. The plan stays your own file: only its path is remembered
(**Put the plan away** stops using it).

**Satellites:** the plan and the carrier labels the decoders learn belong to
the satellite the dish is on — another satellite's carriers at the same
frequencies are other carriers. The satellite list at the top right
(beside **LO trim**) says which one that is: each satellite keeps its own plan and
labels, and only the chosen one's are shown, given to new VFOs and cleared
by **Reset labels**. Type a name in the list and **Add** a satellite (it starts
with no plan or labels); **Rename this one** names the chosen one — the plan
and labels kept before satellites were named are listed as *(unnamed)* until
you name them.

An LNB's oscillator is rarely exactly on 10600 MHz and drifts as it warms,
so the carriers sit a few kHz off their plan frequencies. **LO trim** at the
top right corrects the RF scale for that on the fly (drag or type kHz;
right-click for zero) without retuning anything; **Satellite ▸ ⟲ Align LO trim to the plan** sets it from
the carriers in view — the trim at which most of them land on plan carriers
of like width, searching ±3 MHz — and **keep it aligned** repeats that every 5 s in
small steps as the LNB drifts. The frequency bar shows the trim; it is saved
with the session.

## Text in every decoder

Every decoder shows the text in what it decodes:

- **DVB-S2/S2X and DVB-S → MPEG-TS**: the transport stream's payloads, read
  per PID so a table's strings come out whole — service and provider names,
  EPG text, URLs, anything carried in the clear — and, when the stream
  carries IP (MPE), the IP payloads too.
- **DVB-S2/S2X → GSE/IP** and **TPC 2964 over HDLC**: the UDP/TCP payloads —
  SAP/SDP announcements, HTTP headers, plain-text protocols.
- **TPC 2964** whose payload is not recognised yet, **Identify** while it
  demodulates live, and the **generic PSK** decoder: every reading of the
  bits, as above.

Decoded data may be compressed, encrypted or video, which throws up short
printable runs by chance, so the byte decoders rank strings by how often
they recur (chance runs do not recur) and list long strings as they come.

## What Identify reports

It splits what it *knows* from what it *guesses*. **DVB-S2** is reported as a
fact — PLHEADERs found repeating exactly where their own PLS codes predict the
next frame — with the MODCODs in use, pilots, frame lengths, and CCM vs ACM
(a MODCOD read once among many frames is a misread header, said so, not
taken for ACM or S2X).
Anything else gets measurements (symbol rate, roll-off, an estimated
constellation) and a labelled guess: plain QPSK reads *"possibly DVB-S (not
verified)"* (the DVB-S decoder confirms it), and a BPSK, QPSK or 8PSK carrier
with the TPC 2964 unique word every 2964 bits, or Comtech TPC 7/8's every
16 320, is named as such. So is the **Intelsat 19 DAMA network**: an
outbound by its 176-symbol sync word every 40 112 symbols (looked for
differentially, so it is found even when its pilots make the carrier read
BPSK — it is then called QPSK, and claiming it or the survey runs the IS-19
outbound decoder), a call channel by its pilots, one every 16 symbols in
154-symbol frames (*"Intelsat 19 DAMA call channel … its voice is
encrypted"*: nothing to decode, the outbounds name its calls), and a wide
call channel — 25.6 or 51.2 kBd, in duplex pairs like the voice channels —
by a pilot strictly every 16 symbols. Call channels
sit 6.16 kHz apart, so carrier detection takes the band as one; put a VFO
on one channel (about 6.5 kHz wide) to have it named. For the rest it
looks for fingerprints of proprietary waveforms:

- **DVB-S2-style headers off their own grid**: SOF and PLS headers that recur
  regularly but not where their PLS codes say the next frame starts — S2
  framing whose codes mean something else, as NovelSat NS3/NS4 appear to use
  (their manuals describe S2-style headers, S2 frame sizes and Gold-code
  scrambling, with extra code rates) or a vendor's short frames.
- **A 2 % roll-off**, sharper than DVB-S2X's 5 % — NovelSat NS4's.
- **A convolutional code** on BPSK or QPSK: a rate-½ code makes its coded
  bits satisfy a parity check K steps long whose taps are its generators,
  so Identify finds the check in the hard decisions and names the code
  (*"QPSK, convolutional code K = 7, rate 1/2 (171, 133)"*, or any other up
  to K = 10); the type-found modem and the survey then run the Viterbi
  decoder, which decodes it.
- **Frame structure**: any symbols that repeat frame after frame (a header,
  unique word or pilots) show up in the autocorrelation of the locked
  symbols; Identify reports the period and, given enough frames, where the
  repeating symbols sit (*"repeats every 3330 symbols; 162 known symbols: 90
  from 0, then 2 blocks of 36 every 1476"*) — a fingerprint of a framing even
  when no specification is public.

Comtech VersaFEC and the TPC/LDPC modes of SCPC modems publish no sync
word or frame layout, so they can only be recognised by such measurements;
captures of them would let DecSAT name them — Comtech's TPC 7/8 was worked
out that way (its frames repeating whole while idle gave it away). Paradise FastLink was worked
out that way from a capture of a Q-Flex (QPSK 0.710): Identify names it, and
the **Q-Flex FastLink** decoder decodes it — down to the calls in its TDM
multiplex: G.728 voice spread over four channels, two bits a TDM word.
Where each channel's bits go is learnt from the voice equipment's silence
fill (a known pattern) while the channels idle; the **Call voice** card
then says when someone speaks, and ▶/● play and record it. With **Voice:
play when G.728 speech is found** (on by default) a call plays by itself
once anyone has spoken, taking turns with the Comtech channels under **one
voice at a time**; ⏹ turns that off for the VFO.

The faster Q-Flex links (FastLink at 2, 3 or 4 × the 95 kBd links' rate)
carry 2–4 × 128.5 kbit/s in the same Paradise framing, and the same 257-bit
multiplex at that many times the frame rate, laid out differently (at 3 ×
in superframes of three frames, placed by the alignment column): DecSAT
frames it, meters its activity, and finds in it a 64 kbit/s HDLC timeslot —
bridged Ethernet on an inverted line, the site's IP network — whose
packets go to the IP view and the PCAP like any other IP; then, in another
64 kbit/s timeslot beside it, the radios' calls: G.728 on two bits of each
octet, found by their silence fill. The call speaking is heard, through
the same ▶, ● and *play when G.728 speech is found* as the slower links'
(`decsat payload … --voice-out call.wav` from a data recording).

A carrier too slow to show three frames in the first look is marked
*provisional* while it listens longer. Between identifications it keeps
demodulating with what it found, so the constellation and Es/N0 stay live.

## Snapshots, the session log and PCAP parts

**📷** beside the input level (or **F12**) saves the spectrum and
waterfall as a PNG in the output folder; right-click (or **Shift+F12**) the
whole window. The file is named by the satellite, the RF and the UTC time
(`decsat-<satellite>-<RF>MHz-<UTC>-band.png`) and the picture carries a
caption: time, receiver and rate, RF centre and span, tuning and LNB LO,
satellite and VFOs.

**File ▸ 📝 Session log** writes what the VFOs report to
`decsat-log-<UTC>.csv` in the output folder — UTC time, VFO, RF, decoder,
kind and text: Identify's verdicts, decoder messages, lock and loss (held
2 s, so a flickering lock is one line), each MODCOD when first seen, text
found in the bits (strings that read as text, 30 a minute at most), a
summary a minute, VFOs added and changed, snapshots and the status bar's
notes. A line is written when what is said changes, figures aside, and the
same statement again at most once a minute, so the file stays readable. A
📝 beside the 📷 shows it is on; the choice is remembered.

**File ▸ PCAP parts** starts a new PCAP every 1–60 minutes while one
records (`…-part002.pcap` and on). Each closed part is a whole capture, so
tools that read finished files — BruteShark, NetworkMiner — can take them
while the recording goes on. (Live Wireshark is fed through a pipe to its
standard input, not a network port, so other tools cannot attach to it.)
`decsat decode … --pcap-rotate MIN` does the same unattended.

## Which satellite

A DVB network's NIT gives its transponders' orbital position, and a
DVB-RCS map its forward link's. DecSAT names the satellite there from a
table of 78 positions (177°W–180°E, `apps/decsat-gui/src/orbital_positions.json`):
the TS analyser's network tab and the RCS card show `28.2°E · Astra
2E/2F/2G`, the toolbar shows it in amber beside the satellite list while
another satellite is chosen, and the list offers it first — one click
chooses (or adds) "Astra 2E/2F/2G (28.2°E)", whose plan and carrier labels
then apply. The session log notes each position found.
