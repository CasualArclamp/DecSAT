<!-- made by CasualArclamp -->
# Test signals

Synthetic IQ captures for trying DecSAT without a dish. The captures
themselves are not in git (they are hundreds of MB, and the generator
makes them identically every time); build them here with:

```bash
decsat test-signals tests
```

| File | Rate | What is in it |
|------|------|---------------|
| `decsat-scene_8Msps.cs8` | 8 MS/s, 4 s | A HackRF-style span: DVB-S2 CCM carrying IP over GSE, a DVB-S2/S2X ACM carrier (QPSK → 32APSK) carrying MPEG-TS, DVB-S QPSK 3/4, a TPC 2964 modem carrier carrying IP over HDLC, and a CW tone. |
| `decsat-cid_3360000sps.cs8` | 3.36 MS/s, 30 s | A DVB-S2 QPSK 1/2 carrier (1.12 MBd at +700 kHz, carrying the test transport stream) with a DVB-CID 27.5 dB under it: identifier `75:00:06:B0:FF:FF:01:AC:07`, position 12°45.9′S 23°34.45′E (the examples in ETSI TS 103 129), a telephone number and a text. |

Open either in the GUI (Open IQ…; the sample rate comes from the file name)
and click a carrier, or decode from the command line, e.g.:

```bash
decsat decode tests/decsat-cid_3360000sps.cs8 --decoder cid --offset 700000 --bandwidth 1500000
```

A CID frame takes 17.8 s at 224 kchip/s, so the 30 s capture holds one whole
frame: the identifier and the position. The telephone number and text come
in later frames; for all six (about 110 s):

```bash
decsat cid tests/decsat-cid-long_3360000sps.cs8 --seconds 110
```

`decsat cid --help` lists the rest: identifier, position, telephone, text,
CID level and the host's Es/N0.
