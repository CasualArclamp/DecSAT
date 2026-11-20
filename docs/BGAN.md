<!-- made by CasualArclamp -->
# Inmarsat BGAN forward link (`decsat-bgan`)

A Rust port of [DecBGAN](https://github.com/CasualArclamp/DecBGAN) (Python), run
as a real-time VFO decoder ("Inmarsat BGAN forward"). Bearer **F80T4.5X-8B**:
16-QAM, 151.2 kBd, 189 kHz, 80 ms frames, ETSI TS 102 744-2-1. L-band,
about 1525–1559 MHz downlink; give the VFO 230 kHz or more.

## ETSI tables (needed, not in the repo)

The turbo interleaver and the puncturing/channel-interleaver tables are ETSI
copyright. Download TS 102 744-2-1's Annex C archives from etsi.org and unpack
them so that `ts_1027440201_AnnexC1_v010101p0` and `..._AnnexC2_...` sit in
the working directory (or its parents, `work/`, `annex/`, the exe's folder),
or set `DECSAT_BGAN_ANNEX=<folder>`. The interleaver was checked for a closed
form (QPP/ARP/periodic dither) and none fits, so both tables are read.

## Chain

```
VFO baseband (any rate >= ~215 kS/s)
  -> windowed-sinc resampler to 4 sps -> carrier NCO -> RRC matched filter
  -> AGC -> Gardner timing loop (cubic interpolator; tracks clock error)
  Searching: 2^17 symbols -> 4th-power line = carrier offset; level-agnostic
             folded differential UW search over ~10 frames; lock needs the
             peak in >= 60 % of frames and PSR >= 7
  Locked:    per frame UW search in a +-320 symbol window (offsets step by
             tens of symbols on real recordings), pilot-slope carrier loop,
             per block pilot amplitude + linear phase fit (11 pilots),
             then phase and level again from the block's data symbols,
             level choice, log-MAP turbo decode (8 iterations; 30 at the
             expected level in a file decode; no early stop, rayon across
             blocks), acceptance
  Acceptance: parity agreement > 0.70 AND likelihood-ratio margin > 1200 nats
              (DecBGAN's calibrated pair: zero false accepts over 37281
              impossible tries)
```

Levels: block 0 takes the UW's level; blocks 1-7 take the
ForwardBearerCodeRateParam AVP from block 0's payload (byte 3, clause 5.7.16)
when present, else the UW's level (the spec's default), then a learned trial
order. A level the AVP names is the only one tried. Every trial runs the
full 8 iterations, or 30 at the expected level in a lossless (file) decode;
only when behind real time are levels screened at 3 iterations first, and a
pass is confirmed at 8.

Why no shortcuts: measured over 44000 accepted blocks on nine real
recordings, stopping once an iteration changed no hard decision let 3
corrupt blocks through (stopped at parity agreement 0.73-0.84, where the
finished decode reaches 0.96-0.98, and both acceptance tests passed), and
screening other levels at 6 iterations lost 20 blocks that only converge at
the 7th or 8th. Requiring two unchanged iterations in a row gave no corrupt
blocks on the same set and most of the speed back, but was not adopted:
none in 44000 is not the same as none.

### Weak signals

The forward bearer changes code rate block by block, to suit the terminal
each block is meant for. Blocks for a terminal with a better link than ours
(H1 and up, coded for several dB more than a small dish gets) cannot be
decoded at all. On a real 8 MS/s capture at Es/N0 about 5 dB, most of what
failed was such blocks. The receiver now gets what it can from the rest:

- Phase and level from the data. Each pilot is one symbol, so at 5 dB its
  phase is only good to about 0.3 rad and its magnitude reads high. After
  the pilot fit, the data symbols' fourth power gives the carrier phase of
  each 136-symbol group (to the nearest quarter turn, which the pilots
  settle), and a line is fitted through those. The level comes from the
  block's second and fourth moments.
- Log-MAP instead of max-log-MAP. The LLRs are scaled to each block's
  measured noise. A file decode runs 30 iterations at the expected level;
  a live one runs 8, because with no early stop 30 about doubles the
  decode time.
- File decodes never shed work. `BganRx::set_lossless` (set by the VFO
  when it plays a file as fast as possible) turns off the real-time
  governor, which otherwise drops frames and cuts the level search when
  decoding lags.
- When the AVP names a block's level, the other levels are not tried. On
  real captures this was most of the CPU, and it rescued nothing.

120 s of three carriers on that capture, blocks decoded by the level the
AVP named (`decsat scan` on the whole 16 minutes, before and after, is
below):

| Carrier | Level | Before | After |
|---|---|---|---|
| #0 | L1 | 426 / 585 | 578 / 585 |
| #0 | all | 85.7 % | 87.2 % |
| #8 | R | 15 / 3374 | 215 / 3384 |
| #8 | L1 | 68 / 96 | 90 / 96 |
| #8 | all | 54.9 % | 56.9 % |
| #10 | R | 35 / 284 | 172 / 284 |
| #10 | all | 25.5 % | 26.7 % |
| all | H1, H2, H3 | 0 | 0 |

On the whole 16-minute capture (`decsat scan --decode`), the four carriers'
payloads grew by 1 to 17 %. Their pcaps (full headers restored and
compressed ones rebuilt, as below) grew from 257, 149, 61 and 0 packets to
418, 311, 99 and 15. All but one of the old packets are among them, and one
FEC block that decoded before now fails. The payload grows far less than
the traffic because the newly decoded blocks are R and L1 ones, which carry
the users' packets. The L3 blocks hold mostly signalling and padding.

The decoder fits noise slightly better than before: in 200 000 impossible
tries (random 16-QAM, pure noise, every level) the best parity agreement
was 0.643, up from about 0.60. That is still under the 0.70 acceptance
line, and none of the 200 000 tries was accepted.

These figures are for a file decode. They were measured before the early
stop was removed (see above), so with at most 30 iterations at the expected
level; now each such decode runs all 30.

## Payload to packets

The session writes the accepted blocks' bytes (`…_payload.bin`) and a raw-IP
pcap (`…_ipv4.pcap`). Packets come from two places: `layers/pcap.rs` carves
any IPv4 packet whose header lies contiguous (header checksum must pass, and
by default its TCP, UDP or ICMP checksum too), and `layers/segments.rs`
rebuilds packets the link layer cut into segments, whose header is broken up
by other traffic. The segment format there (connection id, length, sequence
number, first/middle/last flags, CRC-16/X.25 per segment) was worked out from
real recordings, not the spec text; a packet is kept only if every segment's
CRC passes, the sequence numbers run without a gap, and the result has the
declared length and valid IP and transport checksums. On a 23-minute
recording it added 157 packets to the 395 carved.

Two variants of the segment layout are handled. A segment whose first octet
has bit 0x40 (length present, as in the BCtPDU header) clear has no length
octet, and ends where its CRC checks; and some connections put a zero octet
before the IPv4 packet in the SDU (counted in a first segment's SDU length).
Missing either, the carver took a packet cut after its header with the
segment's FCS and other traffic in it: on a 3.5-minute recording 3666 packets
(27 % of the pcap) were corrupt that way. With both, that recording gives no
corrupt packets and 4686 rebuilt ones instead of 1635.

### Header compression

The octet some connections put before the packet is a PDCP packet
identifier (3GPP TS 25.323) for RFC 2507 IP header compression — inferred
from real payloads, not from the BGAN spec. `layers/hc.rs` handles it:

| PID | Packet | Handling |
|---|---|---|
| 0 | no compression | the IPv4 packet as sent |
| 1 | FULL_HEADER | Total Length carries the context id (CID) instead of the length, so the header checksum fails as carried: the length is restored from the SDU's, the sender's checksum must then hold, and the headers become the context |
| 2 | COMPRESSED_TCP | rebuilt from the context with RFC 1144 deltas (special cases included) |
| 3 | COMPRESSED_TCP_NODELTA | rebuilt from the context with the TCP header it carries |
| 4 | COMPRESSED_NON_TCP | rebuilt from the context (UDP or bare IPv4) with the IPv4 id and UDP checksum it carries |

Contexts are kept per link connection and CID, TCP and non-TCP apart. Every
rebuilt packet must pass its transport checksum before it is kept or moves a
context on; a TCP packet that fails is tried with its deltas applied twice
and three times (RFC 2507's "twice" repair for lost packets). Not decoded:
the D bit (compressed RTP or similar on top of UDP), and chains through
tunnel or IPsec headers.

What limits it is loss. A compressed packet whose context was set before the
recording began has nothing to be rebuilt from, and one lost packet with
deltas unlike the next puts a TCP context out of step until the next full
header. On three real recordings (25, 25 and 8 minutes): 36 of 38
compressed non-TCP packets were rebuilt; of 123 compressed TCP packets 24
were, 42 had no context and 57 followed a loss. With full headers restored and
compressed packets rebuilt, the pcaps grew from 161, 37 and 141 packets to
257, 61 and 149.

### Segments that cannot be rebuilt

Many middle and last segments arrive with no SDU open on their connection,
or with a gap in the sequence numbers. On the three recordings above this
was 753, 2620 and 451 segments. For 64 %, 82 % and 69 % of them, the
segment before (sequence number one less) is nowhere in the payload: it was
in an FEC block that failed, almost always one coded above what the dish
received (see *Weak signals*). Most of the rest follow one that was itself
cut off that way. The others belong to connections whose first segment
has no SDU length and no IPv4 header, only data that looks random: not IP
the reassembler could rebuild. None of these were parsing misses. What
helps is decoding more blocks.

To redo an old recording:

```bash
cargo run --release -p decsat-bgan --example bgan_pcap -- X_payload.bin X_full.pcap
```

## Real time

On a 16-thread desktop, 60 s of a 192 kHz recording decodes in 14–34 s
(about 0.25–0.55 of real time; the weakest recording is the slowest, as
blocks that fail are tried at more levels). If decoding ever
falls more than a second behind, frames are dropped and counted instead of
queued, and level search is cut to two candidates when more than 0.3 s behind.

## Verification

`cargo test -p decsat-bgan` runs unit tests. `tests/e2e.rs` builds frames
with the Rust transmitter (`tx.rs`, port of DecBGAN `tx.py`, never touches a
radio), impairs them (noise to 6 dB Es/N0, 3 kHz carrier offset, 30–100 ppm
clock error, fractional timing, resampling from 8 sps, dropped samples) and
checks the decoded payloads **bit-exact**; it needs the Annex tables
(`DECSAT_BGAN_ANNEX`) and skips without them. Real recordings are not
committed; the decoder has been run on nine of them (see Levels above).

```bash
cargo run --release -p decsat-bgan --example bgan_tx -- out/bgan.cf32 20 12 2500
cargo run --release -p decsat-cli -- decode out/bgan.cf32 --rate 604800 --decoder bgan
```

## Finding BGAN carriers

Identify names a BGAN forward carrier when its symbol rate measures within
2 % of 151.2 kBd and the bearer's unique word turns up every 12 096 symbols.
That is the receiver's own acquisition test, so a carrier it names is one the
decoder locks to: the folded, level-agnostic differential UW search
(`decsat_bgan::detect_symbols`, which shares `fold_uw` with `BganRx`), with
the same thresholds, over the 7 or so frames Identify listens to (4 at
least). It needs no Annex tables. The hint reads *"16QAM, Inmarsat BGAN
forward (F80T4.5X-8B: unique word every 12 096 symbols, level L3) — the BGAN
decoder reads it"*. A click on the carrier, the Survey and
`decsat scan --decode` then run the BGAN decoder on a VFO at least 240 kHz
wide (`spec::BGAN_VFO_BW`).

```bash
decsat scan capture.wav --decode --only bgan --out dir
```

This decodes every BGAN carrier in the capture at once, each on a VFO thread
of its own, and prints each one's progress every 5 s. Each carrier writes
`bgan-#N-<offset>-…_ipv4.pcap` and `…_payload.bin` into `dir`. Without
`--out`, what each carrier decoded is reported and nothing is kept. `--only`
takes `decode --decoder`'s names, comma-separated.

Two limits:

- A carrier too weak for Identify's short look (Es/N0 about 2 dB) can go
  unnamed, even though the decoder, put on it by hand, decodes a few
  percent of its blocks.
- A carrier that carrier detection splits into slivers is never given a VFO
  to look at. This was seen on a 500 kS/s capture with BGAN at its centre.
