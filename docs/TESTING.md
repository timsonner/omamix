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

Those cover cubic-to-dB conversion (0.40 → about −24 dB), ±20 dB amplitude, NaN/+∞/−∞ gain (mute or the max boost, never a non-finite amplitude), solo/mute, the ±24 dB input-trim clamp, the effect order, the wheel rule (positive Y raises, one mouse `Line` is 0.5 dB, trackpad points accumulate to a notch, hovered and focused, −∞ one step below −60 dB, a focus click does not move the fader, a direction change or 0.2 s of idle drops the wheel remainder), route keys (stable `application.name` + binary + `media.name` re-attach after a restart; pid and serial only split live duplicates), the "other desktop" graph (a stream with its own strip is excluded; our `client.id`, `omamix.owned`, and any node upstream of Mix Mic never feed Mix Mic; the automatic strip and an application strip with an empty `media.name` refuse every stream of a client that captures Mix Mic; a strip keyed by `media.name` stays on the per-node walk), and the Mix Mic property contract (`media.class = Audio/Source/Virtual`, `priority.session` 1, `priority.driver` 1, `node.latency`, `node.dont-reconnect`, `object.linger=false`). The PipeWire feature tests build a local `pw_properties` object. They do not connect to the daemon and do not create nodes. CI on Ubuntu runs the default-feature commands only: Ubuntu 24.04 apt ships libpipewire 1.0.5.

Headless publish, on this machine only, not from CI. Run it against a private PipeWire, not the session that owns the speakers:

```sh
runtime=$(mktemp -d)
chmod 700 "$runtime"
export XDG_RUNTIME_DIR="$runtime"
export PIPEWIRE_REMOTE="$runtime/pipewire-0"
pipewire -c docs/private-pipewire.conf
```

`docs/private-pipewire.conf` loads protocol-native, `libpipewire-module-access`, metadata, the spa and adapter factories, and link-factory. It does not load ALSA. It starts a dummy driver and a second `Audio/Source` named `private-source`, and it points the private default source at that node so Mix Mic is not the only source. Log the daemon to a file. The socket is `$PIPEWIRE_REMOTE`. Every command below names that socket with `-r` / `--remote` or with `PIPEWIRE_REMOTE` on the same command line. Setting `XDG_RUNTIME_DIR` alone is not enough: a tool that still sees the login session's remote will talk to the speakers.

```sh
cargo run -p omamix --features pipewire -- --headless --mix-mic
```

`--duration SECS` exits on its own. `--tone` writes a 440 Hz sine on the mix engine, which is linked into Mix Mic. Mix Mic configures its own adapter ports at creation (`adapter.auto-port-config` and `node.param.PortConfig` on that node, with monitor outputs), so the ports exist without a session manager. WirePlumber, when it is present, links a recorder that names the source. Without a session manager the ports are still there; the link to `capture_FL` / `capture_FR` has to be created by something else. The recorder stream also stays at PortConfig mode none until that stream gets its own `node.param.PortConfig` (direction Input, mode dsp, with the ports the link will use). Then:

```sh
PIPEWIRE_REMOTE="$XDG_RUNTIME_DIR/pipewire-0" pw-link -r "$PIPEWIRE_REMOTE" \
  omamix.mix-mic:capture_FL pw-record:input_FL
PIPEWIRE_REMOTE="$XDG_RUNTIME_DIR/pipewire-0" pw-link -r "$PIPEWIRE_REMOTE" \
  omamix.mix-mic:capture_FR pw-record:input_FR
```

`pw-record --target` sets a target name and does not set that PortConfig, and it does not create the link by itself. Its remote flag is `--remote` (`-R`). Measure with a session manager present, or after the PortConfig and `pw-link` steps above:

```sh
PIPEWIRE_REMOTE="$XDG_RUNTIME_DIR/pipewire-0" pw-record --remote "$PIPEWIRE_REMOTE" \
  --media-category Capture --target omamix.mix-mic out.wav
```

`parecord` takes `--device=omamix.mix-mic` and does not accept `--target`. Its `-r` flag means record, not a PipeWire remote. Point it at the private Pulse socket with `PULSE_SERVER` and `--server`. Start `pipewire-pulse` on the same `XDG_RUNTIME_DIR` from a copy of the system pulse config whose `server.dbus-name` is `org.omamix.PrivatePulse`, so it does not take the login session's Pulse name. The socket is `unix:$XDG_RUNTIME_DIR/pulse/native`.

```sh
PULSE_SERVER="unix:$XDG_RUNTIME_DIR/pulse/native" parecord --server="$PULSE_SERVER" \
  --device=omamix.mix-mic out.wav
```

A bare `pw_filter` is not a Pulse source, and a recorder aimed at the filter name does not attach to it. The adapter is what makes the Mix Mic target work. It leaves the default source and every other device's volume alone. omamix reads `default.audio.source` and `default.audio.sink` from the PipeWire `default` metadata object and logs when either name changes after the first read. If that source name is `omamix.mix-mic`, including when the name is already stored at startup, it destroys the Mix Mic node right away. A stored choice of Mix Mic does this on every start. Pick another source, then start omamix again. If the source key is never read, Mix Mic is not published.

**Do not run the next command against the session that owns the speakers.** It changes `default.audio.source` on whichever daemon `-r` names. Use the private socket from the startup block above. After Mix Mic is in that private graph, point the private default at it:

```sh
PIPEWIRE_REMOTE="$XDG_RUNTIME_DIR/pipewire-0" pw-metadata -r "$PIPEWIRE_REMOTE" \
  -n default 0 default.audio.source '{ "name": "omamix.mix-mic" }'
```

omamix logs the warning and removes the Mix Mic node. The same command before `omamix --headless --mix-mic` is the stored-at-start case: the warning is logged and Mix Mic is not left published. `default.configured` stays absent.

WirePlumber 0.5.x only-source stays on that private daemon. Copy `docs/omamix-private.conf` to `$XDG_CONFIG_HOME/wireplumber/wireplumber.conf.d/omamix-private.conf` and start `wireplumber -p omamix-private` with the same `XDG_RUNTIME_DIR` and `PIPEWIRE_REMOTE`. That profile inherits `base` and `mixin.stateless`, keeps the session-manager metadata and `policy.standard`, and disables `hardware.audio`, `hardware.bluetooth`, and `hardware.video-capture`, so it does not open the sound card. Publish Mix Mic while `private-source` is the default, then destroy that other private source by id:

```sh
PIPEWIRE_REMOTE="$XDG_RUNTIME_DIR/pipewire-0" pw-cli -r "$PIPEWIRE_REMOTE" destroy <private-source-id>
```

WirePlumber 0.5.17 selects Mix Mic, omamix logs the same warning, and the Mix Mic node is gone. The private metadata dump after that has no `default.configured` key. Read it with `pw-metadata -r "$PIPEWIRE_REMOTE" -n default`, still on the private socket.

Before and after, `wpctl status`, `pactl get-default-source`, `pactl get-default-sink`, and a `pw-dump` node/link list should match. `scripts/unplug-watch.sh` appends those three commands once a second to a log. It does not set a default, a volume, or a config file. It is there for a later physical unplug of the X2u, which this change does not perform.

## Definition of done (v1)

Run these on the laptop after the engine and the UI exist. Record the SHA.

- [ ] **Strips.** Add the Shure X2u, an mpv OST, and a Chromium tab. Set levels. Master limiter on, ceiling −1 dBTP.
- [ ] **Chromium getUserMedia.** On a WebRTC mic test page, select **Mix Mic**. The recording contains voice and music at the balance set on the strips. No silence, no doubled music.
- [ ] **Firefox getUserMedia.** The same WebRTC check in Firefox, with **Mix Mic** selected. Voice and music at the set balance. No silence, no doubled music.
- [ ] **gpu-screen-recorder default source.** Set the default source to Mix Mic (this is the one test that changes the default, and only with Tim doing it). Omarchy screen recording via gpu-screen-recorder captures voice and music. Restore the previous default source afterward. Tim reported that this case failed for `module-remap-source` (silence until the source was named explicitly). This session did not re-run that test.
- [ ] **OBS.** OBS input set to Mix Mic captures the same balance. No silence, no doubling.
- [ ] **Loudness.** The master momentary/short-term LUFS and peak agree with `ffmpeg -i <file> -af ebur128 -f null -` within about 1 dB.
- [ ] **Clean quit.** `pw-dump` to a file, quit omamix, `pw-dump` again. Diff shows no nodes or links left behind. Default sink and default source are unchanged unless the gsr test above changed the source and the restore step has already run.
- [ ] **Wheel.** Scroll while the pointer is on a fader that has been clicked: wheel-up raises it, and each mouse notch moves it 0.5 dB. A trackpad swipe accumulates and does not jump many dB per event. Scroll anywhere else, including after clicking a fader and moving the pointer away: that fader stays put. The click that focuses a fader does not jump its value. One more notch below −60 dB shows `-inf dB`. `0.0 dB` does not render as `-0.0 dB`.
- [ ] **Re-attach.** Save a scene that includes mpv. Quit mpv, start it again, confirm the strip relinks by `application.name` / binary without a new node id being stored.
- [ ] **Review.** Reviewers and Smoke QA sign off on that same SHA before a merge link goes to Tim.

## Out of scope for the scaffold

No PipeWire nodes are created by the tests. Do not point `pw-record`, `pw-link`, or the omamix binary at the default source from CI. The 5 s Shure X2u capture used to write [AUDIO-BASELINE.md](AUDIO-BASELINE.md) was a one-off read of an existing node, targeted by name, and the wav was deleted. `pw-record` briefly linked to that capture node; the link was gone when the process exited.
