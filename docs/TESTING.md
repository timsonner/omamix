# Testing

v1 is done when the checks below pass on XPS15-OMARCHY, on one git SHA. The scaffold only covers the rows marked as unit tests. UI checks are Tim's: the agent does not launch the window.

Rust floor: **1.95** (`rust-toolchain.toml`). Edition 2024 needs >= 1.85; eframe 0.36 is what raises the floor to 1.95. PipeWire feature floor: **libpipewire >= 1.2** (`pkg-config --atleast-version=1.2 libpipewire-0.3`). Older 1.0.x headers fail pipewire-rs 0.10 / libspa `v1_2_0` with E0425 on `spa_meta_sync_timeline`. `crates/omamix-pw/build.rs` enforces that when the feature is on.

## Automated today

```sh
rustc --version   # >= 1.95
pkg-config --atleast-version=1.2 libpipewire-0.3
cargo test --workspace
cargo test -p omamix-pw --features pipewire
cargo test -p omamix --features pipewire
cargo build -p omamix
cargo build -p omamix --features pipewire
```

Those cover cubic-to-dB conversion (0.40 → about −24 dB), ±20 dB amplitude, NaN/+∞/−∞ gain (mute or the max boost, never a non-finite amplitude), solo/mute, the ±24 dB input-trim clamp, per-strip atomics with a 10 ms one-pole (unity-to-mute and gain steps stay under the one-pole jump and settle within 1% after 5 time constants at 44.1 kHz and 48 kHz, balance pan that is unity at center and never above 1 on either side, a default stereo block unchanged on the core path and the filter mix path, solo on an enabled strip ramps the other strips to silence, a disabled strip's solo does not, and `StripMixer::process` allocates nothing), block peak and RMS (a −20 dBFS sine reads −20 ±0.1 dB peak and about −23 dB RMS on the strip and the master; peak and RMS are one snapshot; a block around 1e19 does not read as silence; an enabled strip's meter follows the fader; the callback publishes the block, and `MeterDisplay` holds the peak for 1 s then falls at 20 dB/s), scope rings (a stalled reader does not block the writer, which drops the new frames), the effect order, the wheel rule (positive Y raises, one mouse `Line` is 0.5 dB, trackpad points accumulate to a notch, hovered and focused, −∞ one step below −60 dB, a focus click does not move the fader, a direction change or 0.2 s of idle drops the wheel remainder), route keys (stable `application.name` + binary + `media.name` re-attach after a restart; pid and serial only split live duplicates), the "other desktop" graph (a stream with its own strip is excluded; our `client.id`, `omamix.owned`, and any node upstream of Mix Mic never feed Mix Mic; the automatic strip and an application strip with an empty `media.name` refuse every stream of a client that captures Mix Mic; a strip keyed by `media.name` stays on the per-node walk), and the Mix Mic property contract (`media.class = Audio/Source/Virtual`, `priority.session` 1, `priority.driver` 1, `node.latency`, `node.dont-reconnect`, `object.linger=false`). The PipeWire feature tests build a local `pw_properties` object. They do not connect to the daemon and do not create nodes. CI on Ubuntu runs the default-feature commands only: Ubuntu 24.04 apt ships libpipewire 1.0.5.

## Headless publish

On this machine only, not from CI. From the repository root, in a fresh shell:

```sh
bash scripts/prove.sh
```

`scripts/prove.sh` is the procedure. Execute it. Do not source it. It builds `target/debug/omamix` with `--features pipewire` in the real environment, then runs that binary. It does not call `cargo run`. The binary is `omamix --headless --mix-mic --tone --duration 180`. `--duration 180` keeps Mix Mic up for the later blocks. `--tone` writes a 440 Hz sine on the mix engine. That sine is not a hardware level and not a default gain.

The private socket path on every daemon and client command is the literal `${XDG_RUNTIME_DIR}/omamix-private-0`. The script unsets `PIPEWIRE_REMOTE`, `PIPEWIRE_RUNTIME_DIR`, and `PULSE_RUNTIME_PATH`. It does not load ALSA and it does not open the session that owns the speakers. It does not use tmux.

The private root is a fresh `mktemp -d /tmp/omamix-private.XXXXXX`. Evidence files (daemon logs, recordings, the pactl note, and the private dump) go in a separate fresh `mktemp -d /tmp/omamix-scratch.XXXXXX`. The script refuses `/tmp/omamix-fixround` and anything under `/run/user`. It does not use a fixed `/tmp/omamix-*` name. It exports the private `XDG_RUNTIME_DIR`, `XDG_CONFIG_HOME`, `XDG_STATE_HOME`, `XDG_DATA_HOME`, and `HOME`, and checks that runtime, before it copies any config.

Daemons are children of the script. Their stdout and stderr are log files under the scratch directory. The script does not pass `-v` or `-vv` and does not use `systemd-run`. `PIPEWIRE_LOG_SYSTEMD=false` is set before those daemons start, so PipeWire's own logger does not write the journal. The committed private pipewire and pipewire-pulse configs set `rtkit.enabled = false`, and the script checks that on the private copies. `DISABLE_RTKIT` is set for the private processes so wireplumber and the clients do not call rtkit-daemon either: they load `libpipewire-module-rt` from the system config, where `rtkit.enabled` defaults to true. rtkit-daemon is a system service. This script does not stop it from logging for other sessions. Each pipewire, pipewire-pulse, wireplumber, dbus, and omamix process has a pidfile under the private root. Those pids are not printed. `trap cleanup EXIT INT TERM HUP` tears the daemons down on success, on failure, on Ctrl-C, on SIGTERM, and on SIGHUP. `cleanup` and those traps remove the scratch directory and the private root. `OMAMIX_KEEP_LOGS=1` copies the logs and recordings to a fresh `/tmp/omamix-keep.XXXXXX` directory and prints that path. Without it, nothing is kept.

The before `wpctl status` and the before config-tree listing stay in shell variables. They are not written to files, so a SIGKILL does not leave a status file or a stat file. The login-session check then runs `wpctl status` again with the real runtime, the real home, and with `PIPEWIRE_REMOTE` removed, and keeps that text in a variable too. Each `wpctl status` is a new client, so the comparison ignores that wpctl line and rewrites pids. The script prints the `stat` lines for `~/.config/wireplumber`, `~/.config/pipewire`, and `~/.local/state/wireplumber`. A missing directory is printed as `absent` plus that path, and the run passes when it is still absent afterward. The run fails if that directory appears or if any listing changes. It does not print `wpctl status`. Existing directories' mtimes must be unchanged. If both `wpctl` commands succeed, the rest of the status must be unchanged. If `wpctl` is missing or fails before or after, the live graph was not compared: that is a gap. A private wireplumber that did not start, or that died before cleanup, is a gap.

Exit codes:

- `0`: `private procedure passed`. No gaps.
- `3`: `PASSED WITH GAPS:` and the list. The private captures succeeded, and at least one gap was recorded.
- `1`: a failure. A changed status, a changed mtime, a failed capture, a leftover private process, or a setup error. The script does not print `private procedure passed` or `PASSED WITH GAPS:`.
- `130`: SIGINT. A terminal Ctrl-C during a foreground step, including `pw-record`, is this code. Monitor mode is on only while a background daemon is spawned, so the Ctrl-C reaches the script. The script does not print `private procedure passed`.
- `143`: SIGTERM.
- `129`: SIGHUP.

After cleanup the script runs `pgrep -f` on the private root and prints a count, not pids. It also counts processes whose environment still contains that path. Both counts must be 0.

`--tone` is linked into Mix Mic. Mix Mic configures its own adapter ports at creation (`adapter.auto-port-config` and `node.param.PortConfig` on that node, with monitor outputs), so the ports exist without a session manager. WirePlumber, when it is present, links a recorder that names the source. Without a session manager the ports are still there; the link to `capture_FL` / `capture_FR` has to be created by something else. The recorder stream also stays at PortConfig mode none until that stream gets its own `node.param.PortConfig` (direction Input, mode dsp, with the ports the link will use). `pw-record --target` sets a target name and does not set that PortConfig, and it does not create the link by itself. Its remote flag is `--remote` (`-R`).

The record block starts `pw-record` in the background with `--target 0` (do not link yet) and `-R` set to the literal private socket. It waits until that node exists, sets `PortConfig`, links `omamix.mix-mic:capture_FL` and `capture_FR` while the recorder is running, waits, then stops the recorder. `parecord` is the same shape: background it against `--device=omamix.mix-mic` and `--server` set to the literal private pulse socket, set `PortConfig`, link while it is capturing, wait, then stop. Pass: both files are larger than a wav header, and 440 Hz is the strongest of 100, 220, 440, 880, 1000, and 2000 Hz in each file.

`parecord` does not accept `--target`. Its `-r` flag means record, not a PipeWire remote.

A bare `pw_filter` is not a Pulse source, and a recorder aimed at the filter name does not attach to it. The adapter is what makes the Mix Mic target work. It leaves the default source and every other device's volume alone. omamix reads `default.audio.source` and `default.audio.sink` from the PipeWire `default` metadata object and logs when either name changes after the first read. If that source name is `omamix.mix-mic`, including when the name is already stored at startup, it destroys the Mix Mic node right away. A stored choice of Mix Mic does this on every start. Pick another source, then start omamix again. If the source key is never read, Mix Mic is not published.

After the captures, the script points the private default at Mix Mic with `pw-metadata -r` on the literal private socket. omamix logs the warning and removes the Mix Mic node. `default.configured` stays absent. The script then destroys `private-source` by id on that same socket. Pass: the warning is in the omamix log, no Mix Mic node is left, and `default.configured` is absent. This document does not record a login-session result for that step. The private socket is the only target.

## Unplug test

Tim runs this once, on the login session, after `scripts/prove.sh` has exited. It is read only. It does not set a default, a volume, or a config file. `scripts/unplug-watch.sh` refuses to start when `PIPEWIRE_REMOTE` is set. It pins the `XDG_RUNTIME_DIR` and `HOME` it is given. With no argument it creates a fresh `mktemp` log, prints that path, and appends samples there. An argument still selects the log and is not printed. Pass: the default source name and the default sink name in the last sample match the first sample, and the mtimes of `~/.config/wireplumber`, `~/.config/pipewire`, and `~/.local/state/wireplumber` are unchanged.

```sh
env -u PIPEWIRE_REMOTE -u PIPEWIRE_RUNTIME_DIR -u PULSE_RUNTIME_PATH \
  scripts/unplug-watch.sh
```

Leave that running. Unplug the example USB capture device, wait until the log has a new sample, plug it back in, wait for another sample, then stop the script with Ctrl-C. Compare the first and last `default-source` and `default-sink` lines. Delete the log after comparing. The default log path is outside the repository. `*.log` is gitignored. An example device name is "Shure X2u". That name is not a default and not assumed hardware.

## Definition of done (v1)

Run these on the laptop after the engine and the UI exist. Record the SHA.

- [ ] **Strips.** Add a USB capture device (an example name is "Shure X2u"), an mpv OST, and a Chromium tab. Set levels. Master limiter on, ceiling −1 dBTP. Device names here are examples, not defaults.
- [ ] **Chromium getUserMedia.** On a WebRTC mic test page, select **Mix Mic**. The recording contains voice and music at the balance set on the strips. No silence, no doubled music.
- [ ] **Firefox getUserMedia.** The same WebRTC check in Firefox, with **Mix Mic** selected. Voice and music at the set balance. No silence, no doubled music.
- [ ] **gpu-screen-recorder default source.** Tim sets the default source to Mix Mic, records, then restores the previous source. Omarchy screen recording via gpu-screen-recorder captures voice and music. UNVERIFIED: this session did not re-run that test, and Tim reported that this case failed for `module-remap-source` (silence until the source was named explicitly).
- [ ] **OBS.** OBS input set to Mix Mic captures the same balance. No silence, no doubling.
- [ ] **Loudness.** The master momentary/short-term LUFS and peak agree with `ffmpeg -i <file> -af ebur128 -f null -` within about 1 dB.
- [ ] **Clean quit.** With `PIPEWIRE_REMOTE` unset, `XDG_RUNTIME_DIR` set to the login runtime, and `HOME` set to the login home, dump nodes and links, quit omamix, and dump again. The diff shows no nodes or links left behind. The default sink and default source are unchanged unless the gsr test above changed the source and the restore step has already run. That dump does not call `pw-metadata`.
- [ ] **Wheel.** Scroll while the pointer is on a fader that has been clicked: wheel-up raises it, and each mouse notch moves it 0.5 dB. A trackpad swipe accumulates and does not jump many dB per event. Scroll anywhere else, including after clicking a fader and moving the pointer away: that fader stays put. The click that focuses a fader does not jump its value. One more notch below −60 dB shows `-inf dB`. `0.0 dB` does not render as `-0.0 dB`.
- [ ] **Re-attach.** Save a scene that includes mpv. Quit mpv, start it again, confirm the strip relinks by `application.name` / binary without a new node id being stored.
- [ ] **Review.** Reviewers and Smoke QA sign off on that same SHA before a merge link goes to Tim.

## Out of scope for the scaffold

No PipeWire nodes are created by the unit tests. Do not point `pw-record`, `pw-link`, or the omamix binary at the default source from CI. The 5 s capture used to write [AUDIO-BASELINE.md](AUDIO-BASELINE.md) was a one-off read of an existing node, targeted by name, and the wav was deleted. `pw-record` briefly linked to that capture node; the link was gone when the process exited. That capture was measured on one machine. It is an example baseline, not a default gain.
