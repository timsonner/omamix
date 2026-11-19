# omamix

omamix is a PipeWire live mixer (version 0.1.0) for combining a capture device and per-app playback into one virtual microphone, Mix Mic, that other programs can record. The window is pre-alpha: it draws one placeholder strip and does not publish that node. A build with `--features pipewire`, run as `omamix --headless --mix-mic`, publishes Mix Mic (`media.class` `Audio/Source/Virtual`, `node.name` `omamix.mix-mic`, `priority.session` 1) and removes it on exit. If that name is the default source, including a choice already stored at startup, omamix destroys the node. If the default source cannot be read, Mix Mic is not published.

omamix targets any Linux workstation with PipeWire and working sound drivers. It does not assume a particular microphone, interface, or laptop. The hostnames, devices, and levels in [docs/AUDIO-BASELINE.md](docs/AUDIO-BASELINE.md) come from one example machine and are not defaults.

## Processing defaults

- Strip trim and fader start at 0 dB, pan at center, mute and solo off, strip enabled. The master fader starts at 0 dB. A 10 ms one-pole ramps later steps so they do not click.
- Each strip and the master publish block peak and RMS in dBFS. A sine at -20 dBFS reads -20 peak and about -23 RMS. Silence stays at -inf.
- The high-pass and the 10-band graphic EQ default to flat (every band at 0 dB, high-pass off). The bands are peaking sections, not shelves.
- The compressor on each strip and on the master defaults to bypass. Its generic settings are threshold -18 dB, ratio 4:1, attack 10 ms, release 100 ms, hard knee, makeup 0 dB. Gain reduction is published per strip and for the master.
- The master limiter defaults to on: ceiling -1 dBTP, lookahead 5 ms (range 0 to 20 ms, where 0 ms is a clipper). Reported latency is that lookahead, published as ProcessLatency and port Latency. A line `lookahead <ms>` on stdin sets it. [`scripts/true-peak-proof.sh`](scripts/true-peak-proof.sh) checks the ceiling with ffmpeg.

These rules have unit tests that do not connect to PipeWire.

## Prerequisites

- Rust **1.95** or newer. [`rust-toolchain.toml`](rust-toolchain.toml) selects the 1.95 channel, and `rust-version` is 1.95 because eframe 0.36 requires it. Edition 2024 itself needs 1.85.
- A display for the window. On X11 the process loads `libxkbcommon-x11.so.0` before the window opens. It also loads Wayland, libxkbcommon, and EGL (`libEGL.so.1`).
- For `--features pipewire`: `pkg-config`, clang, libclang, and **libpipewire >= 1.2** (`pkg-config --atleast-version=1.2 libpipewire-0.3`). pipewire-rs 0.10 with feature `v1_2_0` fails on libspa 1.0.x. [`crates/omamix-pw/build.rs`](crates/omamix-pw/build.rs) runs that check. Ubuntu 24.04 apt ships `libpipewire-0.3-dev` 1.0.5, which is too old. `cargo test --workspace` leaves the feature off, so it needs neither the library nor a running daemon.

Arch:

```sh
sudo pacman -S rustup clang pkgconf pipewire libpipewire wayland libxkbcommon libxkbcommon-x11 mesa
```

Debian/Ubuntu (install `libpipewire-0.3-dev` only when that package is >= 1.2):

```sh
sudo apt install clang libclang-dev pkg-config libwayland-dev libxkbcommon-dev libxkbcommon-x11-0 libegl1 libgl1-mesa-dev
```

## Build, test, and run

From the repo root. These match [`.github/workflows/ci.yml`](.github/workflows/ci.yml), plus the agnostic guard and the local window:

```sh
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
bash scripts/check-agnostic.sh
cargo build -p omamix
cargo run -p omamix
```

`cargo run -p omamix` opens the window. It does not publish Mix Mic. `--help` exits before any window:

```sh
cargo run -p omamix -- --help
```

With libpipewire >= 1.2:

```sh
cargo test -p omamix-pw --features pipewire
cargo test -p omamix --features pipewire
cargo build -p omamix --features pipewire
cargo run -p omamix --features pipewire -- --headless --mix-mic
```

Those feature tests build a local `pw_properties` object. They do not connect to a daemon. [`scripts/prove.sh`](scripts/prove.sh) is the private-socket check (execute it; do not source it). It builds with `--features pipewire --offline`, so fetch dependencies first, and it needs PipeWire, WirePlumber, pipewire-pulse, `pw-record`, and `parecord`:

```sh
bash scripts/prove.sh
```

The Arch package is `omamix-git` ([`PKGBUILD`](PKGBUILD)). `build` runs `cargo build --frozen --release -p omamix --features pipewire` and `package` installs `/usr/bin/omamix`. Its `source` is the unpinned public GitHub repo.

## Usage

```text
omamix [--headless] [--mix-mic] [--duration SECS] [--monitor-mix] [--tone]
       [--strips other] [--strips device:NODE] [--strips app:NAME:BINARY]
```

`--headless` does not open a window. On a build without `--features pipewire` it exits and opens no audio client. `--mix-mic`, `--strips`, `--monitor-mix`, `--tone`, and `--duration` require `--headless`. `--duration 2` stops the process after two seconds. `--tone` writes a 440 Hz sine on Mix Mic for a recorder aimed at that node. `--monitor-mix` also publishes the monitor playback stream. `--strips` may be repeated: `device:` is an ALSA `node.name`, and `app:NAME:BINARY` is `application.name` and `application.process.binary`.

```sh
cargo run -p omamix --features pipewire -- --headless --mix-mic --duration 2
cargo run -p omamix --features pipewire -- --headless --mix-mic --tone --duration 2 --monitor-mix --strips other
```

To add your own mic, find its PipeWire `node.name` and pass it as a device strip. omamix has no default device:

```sh
wpctl status
pw-cli ls Node
cargo run -p omamix --features pipewire -- --headless --mix-mic \
  --strips device:alsa_input.usb-Your_Vendor_Your_Product-00.analog-stereo
```

In the window, click a fader, then scroll while the pointer is still on it. One notch is 0.5 dB. Strip trim and fader start at 0 dB, pan at center (both channels stay at gain 1), with mute and solo off. The master fader starts at 0 dB.

## Layout

```text
crates/omamix-core/   scene, dB math, fader, meters, DSP (no PipeWire)
crates/omamix-pw/     audio engine; PipeWire filter and headless loop behind the feature
crates/omamix/        eframe binary
docs/                 spec, architecture, testing, audio baseline
scripts/prove.sh      private PipeWire procedure
scripts/true-peak-proof.sh  limiter ceiling check
scripts/check-agnostic.sh   CI guard against machine-specific names in code
PKGBUILD              Arch package omamix-git
```

- [docs/SPEC.md](docs/SPEC.md): product spec
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md): UI toolkit, PipeWire approach, example host snapshot
- [docs/TESTING.md](docs/TESTING.md): v1 definition of done
- [docs/AUDIO-BASELINE.md](docs/AUDIO-BASELINE.md): read-only measurement from one example laptop (devices and levels are not defaults)

## License

MIT. See [LICENSE](LICENSE).
