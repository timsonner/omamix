# omamix

PipeWire live mixer for Omarchy. It will take a mic and per-app playback, set levels and processing, and publish one virtual microphone other apps can record.

**Status: pre-alpha.** This tree is a scaffold. The window is a placeholder strip, the DSP chain is a trait plus a gain stage, and the process does not connect to PipeWire unless a later change calls the backend. Levels, routing keys, and the fader wheel rule have unit tests. There is no virtual mic yet.

## Build

Rust **1.95 or newer** (`rust-toolchain.toml` selects the 1.95 channel, any 1.95.x; edition 2024 itself needs 1.85, and eframe 0.36 requires 1.95). This machine is on 1.99, which still builds. libpipewire **>= 1.2** is required for `--features pipewire`. pipewire-rs 0.10 with feature `v1_2_0` fails on 1.0.x with E0425 (`spa_meta_sync_timeline`). Ubuntu 24.04's apt package is libpipewire 1.0.5, which is too old. `crates/omamix-pw/build.rs` runs `$PKG_CONFIG` (or `pkg-config`) `--atleast-version=1.2 libpipewire-0.3` and stops with that explanation. CI (`.github/workflows/ci.yml`) runs fmt, clippy, and tests with the default features only, for that Ubuntu package reason.

From the repo root:

```sh
cargo test --workspace
cargo build -p omamix
cargo build -p omamix --features pipewire
cargo test -p omamix-pw --features pipewire
cargo test -p omamix --features pipewire
```

`cargo test --workspace` does not enable the `pipewire` feature, so it does not need a running daemon or libpipewire.

Build dependencies for the PipeWire feature and the egui binary:

| | Packages |
| --- | --- |
| Arch | `rustup` (or `rust`), `clang`, `pkgconf`, `pipewire`, `libpipewire` (>= 1.2), `wayland`, `libxkbcommon`, `mesa` |
| Debian/Ubuntu | rustup for rustc >= 1.95, plus `clang`, `libclang-dev`, `pkg-config`, `libpipewire-0.3-dev` (>= 1.2; Ubuntu 24.04 apt is 1.0.5 and is too old), `libwayland-dev`, `libxkbcommon-dev`, `libgl1-mesa-dev` |

Arch example: `sudo pacman -S rustup clang pkgconf pipewire libpipewire wayland libxkbcommon mesa`

Debian/Ubuntu example: `sudo apt install clang libclang-dev pkg-config libpipewire-0.3-dev libwayland-dev libxkbcommon-dev libgl1-mesa-dev`

Run the window locally with `cargo run -p omamix`. UI checks are Tim's; this scaffold was not launched from the agent.

## Dev machine

XPS15-OMARCHY: Arch-based Omarchy, Hyprland/Wayland, PipeWire 1.6.8 + WirePlumber 0.5.17 + pipewire-pulse. Measured levels and the graph are in [docs/AUDIO-BASELINE.md](docs/AUDIO-BASELINE.md). Do not change the default sink or source, Easy Effects (if it is installed again), Hyprland binds, or WirePlumber/PipeWire config from an agent session.

## Docs

- [docs/SPEC.md](docs/SPEC.md) — product spec
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — UI toolkit, PipeWire approach, what this machine has installed
- [docs/TESTING.md](docs/TESTING.md) — v1 definition of done
- [docs/AUDIO-BASELINE.md](docs/AUDIO-BASELINE.md) — read-only measurement of this laptop's audio graph

Packaging is a `PKGBUILD` in the repo. It lists Arch repo packages only, no AUR dependencies. It is a dev stub: `source` is the Origin git URL, `prepare` runs `cargo fetch --locked`, and `build` runs `cargo build --frozen`.

## License

omamix is MIT-licensed. See [LICENSE](LICENSE).
