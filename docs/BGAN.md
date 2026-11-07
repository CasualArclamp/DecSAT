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
             level choice, max-log-MAP turbo decode (8 iterations, early
             stop, rayon across blocks), acceptance
  Acceptance: parity agreement > 0.70 AND likelihood-ratio margin > 1200 nats
              (DecBGAN's calibrated pair: zero false accepts over 37281
              impossible tries)
```

Levels: block 0 takes the UW's level; blocks 1-7 take the
ForwardBearerCodeRateParam AVP from block 0's payload (byte 3, clause 5.7.16)
when present, else the UW's level (the spec's default), then a learned trial
order. A wrong-level trial runs 4 iterations, a near miss is finished.

## Real time

Decoding is about 15–20 % of one core's worth of real time at 12 dB Es/N0
(measured through the VFO, `decsat decode … --decoder bgan`). If decoding ever
falls more than a second behind, frames are dropped and counted instead of
queued, and level search is cut to two candidates when more than 0.3 s behind.

## Verification

`cargo test -p decsat-bgan` runs unit tests. `tests/e2e.rs` builds frames
with the Rust transmitter (`tx.rs`, port of DecBGAN `tx.py`, never touches a
radio), impairs them (noise to 6 dB Es/N0, 3 kHz carrier offset, 30–100 ppm
clock error, fractional timing, resampling from 8 sps, dropped samples) and
checks the decoded payloads **bit-exact**; it needs the Annex tables
(`DECSAT_BGAN_ANNEX`) and skips without them. Not yet checked against a real
BGAN recording in this repository (none is committed).

```bash
cargo run --release -p decsat-bgan --example bgan_tx -- out/bgan.cf32 20 12 2500
cargo run --release -p decsat-cli -- decode out/bgan.cf32 --rate 604800 --decoder bgan
```
