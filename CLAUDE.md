<!-- made by CasualArclamp -->
# DecSAT — working notes

Read [`docs/DESIGN.md`](docs/DESIGN.md) first: it holds the agreed scope,
the crate layout and the milestone plan. Update it when a decision changes.

## Commands

```bash
cargo check --workspace --all-targets   # fast feedback
cargo test --workspace                  # unit tests
cargo clippy --workspace --all-targets -- -D warnings
cargo run --release -p decsat-gui       # the GUI
cargo run --release -p decsat-cli -- modcods
```

libopus (Opus radio) is the `third_party/opus` submodule (tag v1.6.1), built
by `decsat-opus-sys` with CMake, as in DecDRM; libxaac (AAC with SBR/PS,
Apache-2.0) is `third_party/libxaac` (v0.1.13), its decoder built by
`decsat-xaac-sys` with `cc`. Not FDK-AAC: its licence does not mix with the
GPL. `git submodule update --init` fetches both.

The live HackRF front end is behind `--features hackrf` (on `decsat-io`, re-exported
by both apps), so a plain build never needs the HackRF SDK and CI stays green.
The Fobos SDR is behind `--features fobos` (on by default in both apps): it
loads RigExpert's `fobos_sdr`/`fobos` library at run time, so building needs
nothing extra. `DECSAT_FOBOS_LIB=/path/to/lib cargo test -p decsat-io
--features fobos live_fobos -- --ignored` runs the live test against a board
(or a stand-in library).
The Airspy Mini / R2 is behind `--features airspy` (on by default in both
apps): pure Rust over USB through `libairspy-rs`, no DLL.

## House rules

- **Licence:** GPL-3.0-or-later. The project borrows algorithms from GPL-3 code
  (gr-dvbs2rx, leansdr, gr-dtv) and MIT code (dontlookup). Credit the source in a
  comment at the function or module that borrows from it.
- **Never commit** IQ captures, reference clones (`reference/`), or built exes.
- **Specs are the authority:** ETSI EN 302 307-1 / -2. Cite the clause or table
  number in comments next to constants, tables and bit layouts.
- The user has a strong DSP background but is newer to Rust: explain Rust idioms
  in comments, not DSP basics.
- DSP is unusably slow unoptimised, so `[profile.dev]` runs at `opt-level = 1`
  with dependencies at 3. Prefer `--release` for anything timing-sensitive.
- Heavy test sweeps belong on CI, not on the user's machine.

## Release

See the project memory: bump `[workspace.package].version` by matching the
`version = "…"` line, commit, wait for CI, annotated tag, push the tag; the
release workflow builds the portable static-CRT exes and drafts the release.
