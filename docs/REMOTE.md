<!-- made by CasualArclamp -->
# DecSAT remote protocol

A radio on one machine, DecSAT on another, over one TCP connection (default
port 5262). The server is receive only: nothing in it opens a transmit path.

| | runs on the server | crosses the link | client |
|---|---|---|---|
| **Dumb** | the radio | packed IQ | a normal engine on `RemoteIqSource` |
| **Smart** | the radio, the engine, the decoders | waterfall, spectrum, carriers, decoder status, constellations, TS | `RemoteEngine`, which has `Engine`'s methods |
| **Shared** | as smart, the radio fixed | as smart, per user | `RemoteEngine` |

Smart mode currently runs Identify, DVB-S2/S2X → TS and DVB-S → TS. Other
decoders are refused with a message; they still work in dumb mode, where
the decoding is local.

## Running it

```bash
decsat-server --bind 0.0.0.0:5262 --password-file secret.txt
decsat-server --bind 0.0.0.0:5262 --shared hackrf --center 739.5e6 --rate 8e6 --gain lna=24 --max-each 2 --max-total 8
decsat-server --file capture.cs8 --file-rate 8e6 --file-center 739.5e6   # a capture as a radio
```

`decsat-server-app.exe` is the same server in a window, for double-clicking: it shows the address to connect to and who is connected, and keeps its settings in `%APPDATA%\DecSAT-Server\prefs.txt` (the password only if asked to, as plain text).

The default bind is 127.0.0.1. A password can also come from
`DECSAT_PASSWORD`. No password is the default, and the server says so if it
is also listening beyond the machine.

## Security

The password is optional. When set, the server sends a 32-byte random nonce
and the client answers with
`HMAC-SHA-256(password, "decsat-remote-auth-v1" ‖ server nonce ‖ client nonce)`,
so the password does not cross the wire and an answer cannot be replayed.

The link itself is **not encrypted**, so a recorded handshake can be attacked
offline with password guesses, and the BISS key of a smart-mode VFO crosses
in the clear. Over the internet, use a VPN or an SSH tunnel. On a shared
server BISS keys are ignored, since campers would receive the descrambled
stream.

## Measured

On `tests/decdvb-scene_8Msps.cs8` (8 MS/s, four carriers), over loopback:

| | link | carrier at +1.2 MHz (0.9 Mbit/s TS) |
|---|---|---|
| Smart, no VFO but Identify | 0.3–0.6 Mbit/s | |
| Smart, DVB-S2 → TS | 1.5 Mbit/s | 25.1 dB MER, 0 frames lost |
| Dumb, 8 bits | 128 Mbit/s | Es/N0 25.0 dB, 401 of 401 frames |
| Dumb, 4 bits | 64 Mbit/s | Es/N0 21.9 dB, 400 of 400 frames |

The Es/N0 and MER values are single readings at the end of each run, on whichever MODCOD the last frame used, so do not read the 8-bit and 4-bit difference as the cost of 4 bits. The dumb link figures are the arithmetic for 8 MS/s at that width; zstd was on and
its saving on this capture was not measured. Not yet measured on a real
capture or on ARM64.

## Framing

All integers little-endian.

```text
u32 length      counts kind + flags + payload; at most 8 MiB
u8  kind        0 control, 1 IQ, 2 row, 3 spectrum, 4 front, 5 VFO, 6 TS
u8  flags       bit 0: payload is one zstd frame
payload
```

The client opens with the four bytes `DSR1`, then frames:
`Hello` → `ServerHello` → (`Auth`) → `Ready`, then `Start`. Control, front
and VFO payloads are [postcard](https://postcard.jamesmunns.com/)-encoded
`C2S`/`S2C`/`FrontWire`/`VfoWire` (`crates/decsat-remote/src/msg.rs`). Those
reuse the engine's own status types, so `PROTO` in `wire.rs` is bumped when
any of them changes, and a mismatch is refused during the handshake.

A client pings every 2 s. Either end treats 15 s of silence as a dead link.

## IQ payload (dumb)

```text
u64 first_sample    stream position, so a gap shows as a jump
u32 samples
u8  bits            4, 6, 8, 10, 12 or 16
i8  gain[ceil(samples / 1024)]
packed components   I then Q, two's complement, MSB first
```

The word is `x · 2^(gain/4) · 2^(bits−1)`, saturated. At 8 bits and above the
gain is a power of two set from the block's peak, so nothing clips and integer
sources stay exact (a HackRF's `v/128` at 8 bits comes back unchanged). Below
8 bits a peak-scaled word would be mostly unused on noise-like signals, so
full scale is loaded at 3.5σ in 1.5 dB steps, and rare peaks saturate
deliberately.

Measured signal to quantisation noise on Gaussian input, as a floor the test
enforces (`iq::tests`): 4 bits ≥ 13 dB, 6 ≥ 26, 8 ≥ 26, 10 ≥ 38, 12 ≥ 50,
16 ≥ 74. The 4-bit figure allows for the 1.5 dB step and the clipping. These
are against the *whole* band's power; a weak carrier's own SNR suffers only
if the quantisation noise approaches its noise floor.

zstd rarely gains much on real IQ, which is near noise in its low bits; it
helps when the signal does not fill the word. The frame is sent uncompressed
when zstd does not make it smaller.

A slice (`IqParams::slice`) runs the same DDC the VFOs use, so the stream's
rate is the DDC's output rate and its centre is moved by the offset.

## Display (smart)

Rows are quantised to bytes on a fixed scale (−160 dB, 0.75 dB steps), delta
coded against the last row actually sent, and zstd compressed. A row is 48
averaged FFTs, whose spread is smaller than a step, so most deltas are zero.

```text
u64 seq, f32 floor, f32 step, u8 key, bytes[n]
```

The spectrum uses the same layout at 0.25 dB steps, on a floor that follows
the trace in 16 dB jumps (a jump sends a key trace). Constellations are signed
bytes against a common scale, at most `scatter_points` a status.

## TS (smart)

```text
u32 vfo, u32 lost, then runs of: varint nulls, varint count, count × 188 bytes
```

Null packets and PIDs outside the client's filter are counted, not sent; the
client puts back nulls, so the multiplex keeps its rate and its PCR spacing.
PSI (PIDs 0, 1, 0x10, 0x11, 0x12, 0x14) always passes the filter. `lost` is
what the server dropped under congestion before this frame.

The client runs the TS analyser on what it receives, so the service list is
not sent as status. With a PID filter on, it knows only the PIDs that arrive.
The counters beside it (CRC, CC errors, rate) are the server's and cover the
whole stream.

TS recording, UDP/TCP re-serving and the in-app player are all on the client.

## Congestion

One TCP stream, so the order of frames is the only lever:
control, then TS or IQ, then front, VFO status, spectrum, rows. Every 16th
frame the display goes first, so a TS filling the link cannot freeze the
waterfall. Front, status and spectrum are latest-wins. Rows queue up to 16;
when 6 are waiting, the rate halves (down to 1 in 16), and it recovers after
3 s of clear queue. TS beyond 16 MiB of queue is dropped and reported. In dumb
mode whole IQ frames are dropped once 250 ms is queued (at least 4 MiB), and
show up as a jump in `first_sample`.

The socket send buffer is 512 kiB in smart mode, so these queues and not the
kernel decide the order, and 8 MiB in dumb mode.

## Shared mode

The server opens its radio at start and it stays open. Every user sees the
same band. A user can add decoders up to `--max-each` (and the server's
`--max-total`), and camp on anyone's. Only a decoder's owner can change it;
campers follow the owner's retunes. If the owner leaves the decoder runs on
without an owner while anyone is camped on it, and closes with the last
camper. Tuning, gain, FFT size, pause and DC removal are refused or ignored.
