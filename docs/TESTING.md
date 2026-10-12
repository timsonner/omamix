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

Headless publish, on this machine only, not from CI. Run the blocks below from the repository root, in order, in one shell. They start a private PipeWire whose socket is `omamix-private-0`. They do not load ALSA and they do not open the session that owns the speakers. `${VAR:?}` aborts if that variable is unset. The login-session check is `wpctl status` with the real runtime directory, the real home, and with `PIPEWIRE_REMOTE` removed.

```sh
set -u
set -m
OMAMIX_REPO=${PWD:?}
OMAMIX_REAL_HOME=${HOME:?}
OMAMIX_REAL_RUNTIME=${XDG_RUNTIME_DIR:?}
OMAMIX_PRIVATE_ROOT=$(mktemp -d /tmp/omamix-private.XXXXXX)
: "${OMAMIX_PRIVATE_ROOT:?private root unset}"
case "${OMAMIX_PRIVATE_ROOT:?}" in
  /run/user|/run/user/*)
    echo "private root must not sit under /run/user" >&2
    exit 1
    ;;
esac
if [ "${OMAMIX_PRIVATE_ROOT:?}" = "${OMAMIX_REAL_HOME:?}" ]; then
  echo "private root must not be the real home" >&2
  exit 1
fi
install -d -m 700 \
  "${OMAMIX_PRIVATE_ROOT:?}/runtime" \
  "${OMAMIX_PRIVATE_ROOT:?}/config/wireplumber/wireplumber.conf.d" \
  "${OMAMIX_PRIVATE_ROOT:?}/config/pipewire" \
  "${OMAMIX_PRIVATE_ROOT:?}/state" \
  "${OMAMIX_PRIVATE_ROOT:?}/data" \
  "${OMAMIX_PRIVATE_ROOT:?}/home" \
  "${OMAMIX_PRIVATE_ROOT:?}/log" \
  "${OMAMIX_PRIVATE_ROOT:?}/run" \
  "${OMAMIX_PRIVATE_ROOT:?}/out"
cp "${OMAMIX_REPO:?}/docs/omamix-private.conf" \
  "${OMAMIX_PRIVATE_ROOT:?}/config/wireplumber/wireplumber.conf.d/omamix-private.conf"
cp "${OMAMIX_REPO:?}/docs/omamix-private-pulse.conf" \
  "${OMAMIX_PRIVATE_ROOT:?}/config/pipewire/omamix-private-pulse.conf"
export XDG_RUNTIME_DIR="${OMAMIX_PRIVATE_ROOT:?}/runtime"
export XDG_CONFIG_HOME="${OMAMIX_PRIVATE_ROOT:?}/config"
export XDG_STATE_HOME="${OMAMIX_PRIVATE_ROOT:?}/state"
export XDG_DATA_HOME="${OMAMIX_PRIVATE_ROOT:?}/data"
export HOME="${OMAMIX_PRIVATE_ROOT:?}/home"
unset PIPEWIRE_REMOTE
export PIPEWIRE_REMOTE="${XDG_RUNTIME_DIR:?}/omamix-private-0"
export PULSE_SERVER="unix:${XDG_RUNTIME_DIR:?}/pulse/omamix-private-pulse"
case "${XDG_RUNTIME_DIR:?}" in
  /run/user|/run/user/*)
    echo "private runtime must not sit under /run/user" >&2
    exit 1
    ;;
esac
case "${PIPEWIRE_REMOTE:?}" in
  /run/user|/run/user/*)
    echo "private socket must not sit under /run/user" >&2
    exit 1
    ;;
esac
stat -c '%n %Y' \
  "${OMAMIX_REAL_HOME:?}/.config/wireplumber" \
  "${OMAMIX_REAL_HOME:?}/.config/pipewire" \
  "${OMAMIX_REAL_HOME:?}/.local/state/wireplumber"
find "${OMAMIX_REAL_HOME:?}/.config/wireplumber" "${OMAMIX_REAL_HOME:?}/.config/pipewire" "${OMAMIX_REAL_HOME:?}/.local/state/wireplumber" -printf '%p %T@ %s\n' | sort > /tmp/omamix-config-before.txt
sha256sum /tmp/omamix-config-before.txt
env -u PIPEWIRE_REMOTE \
  XDG_RUNTIME_DIR="${OMAMIX_REAL_RUNTIME:?}" \
  HOME="${OMAMIX_REAL_HOME:?}" \
  wpctl status
cleanup() {
  root=${OMAMIX_PRIVATE_ROOT:?}
  for pidfile in "${root:?}"/run/*.pid; do
    if [ -f "${pidfile:?}" ]; then
      pid=$(cat "${pidfile:?}")
      kill -- "-${pid:?}" 2>/dev/null || kill "${pid:?}" 2>/dev/null || true
    fi
  done
  rm -rf /tmp/omamix-private-log /tmp/omamix-private-out
  cp -a "${root:?}/log" /tmp/omamix-private-log 2>/dev/null || true
  cp -a "${root:?}/out" /tmp/omamix-private-out 2>/dev/null || true
  rm -rf "${root:?}"
  tmux kill-session -t omamix-priv 2>/dev/null || true
}
trap cleanup EXIT
```

`docs/private-pipewire.conf` names the core and the native socket `omamix-private-0`. It loads protocol-native, `libpipewire-module-access`, metadata, the spa and adapter factories, and link-factory. It does not load ALSA. It starts a dummy driver, an `Audio/Source` named `private-source`, and an `Audio/Sink` named `private-sink`, and it points the private defaults at those nodes so Mix Mic is not the only source. The WirePlumber profile is copied only to `${OMAMIX_PRIVATE_ROOT:?}/config/wireplumber/wireplumber.conf.d/omamix-private.conf`. It inherits `base` and `mixin.stateless`, keeps the session-manager metadata and `policy.standard`, and disables `hardware.audio`, `hardware.bluetooth`, and `hardware.video-capture`. The pulse config is copied to `${OMAMIX_PRIVATE_ROOT:?}/config/pipewire/omamix-private-pulse.conf`. Its `server.dbus-name` is `org.omamix.PrivatePulse` and its listen address is `unix:omamix-private-pulse`, which pipewire-pulse creates at `${XDG_RUNTIME_DIR:?}/pulse/omamix-private-pulse`.

```sh
XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:?}" \
HOME="${HOME:?}" \
dbus-daemon --session \
  --address="unix:path=${XDG_RUNTIME_DIR:?}/omamix-private-bus" \
  --nofork --nopidfile \
  > "${OMAMIX_PRIVATE_ROOT:?}/log/dbus.log" 2>&1 &
echo $! > "${OMAMIX_PRIVATE_ROOT:?}/run/dbus.pid"
export DBUS_SESSION_BUS_ADDRESS="unix:path=${XDG_RUNTIME_DIR:?}/omamix-private-bus"
```

```sh
case "${XDG_RUNTIME_DIR:?}" in
  /run/user|/run/user/*)
    echo "refusing to start pipewire under /run/user" >&2
    exit 1
    ;;
esac
XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:?}" \
HOME="${HOME:?}" \
DBUS_SESSION_BUS_ADDRESS="${DBUS_SESSION_BUS_ADDRESS:?}" \
PIPEWIRE_REMOTE="${XDG_RUNTIME_DIR:?}/omamix-private-0" \
pipewire -c "${OMAMIX_REPO:?}/docs/private-pipewire.conf" \
  > "${OMAMIX_PRIVATE_ROOT:?}/log/pipewire.log" 2>&1 &
echo $! > "${OMAMIX_PRIVATE_ROOT:?}/run/pipewire.pid"
for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
  if [ -S "${PIPEWIRE_REMOTE:?}" ]; then
    break
  fi
  sleep 0.2
done
if [ ! -S "${PIPEWIRE_REMOTE:?}" ]; then
  echo "private socket was not created" >&2
  exit 1
fi
if [ -S "${XDG_RUNTIME_DIR:?}/pipewire-0" ]; then
  echo "private runtime created pipewire-0" >&2
  exit 1
fi
```

```sh
XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:?}" \
XDG_CONFIG_HOME="${XDG_CONFIG_HOME:?}" \
XDG_STATE_HOME="${XDG_STATE_HOME:?}" \
XDG_DATA_HOME="${XDG_DATA_HOME:?}" \
HOME="${HOME:?}" \
DBUS_SESSION_BUS_ADDRESS="${DBUS_SESSION_BUS_ADDRESS:?}" \
PIPEWIRE_REMOTE="${PIPEWIRE_REMOTE:?}" \
wireplumber -p omamix-private \
  > "${OMAMIX_PRIVATE_ROOT:?}/log/wireplumber.log" 2>&1 &
echo $! > "${OMAMIX_PRIVATE_ROOT:?}/run/wireplumber.pid"
```

```sh
XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:?}" \
XDG_CONFIG_HOME="${XDG_CONFIG_HOME:?}" \
XDG_STATE_HOME="${XDG_STATE_HOME:?}" \
XDG_DATA_HOME="${XDG_DATA_HOME:?}" \
HOME="${HOME:?}" \
DBUS_SESSION_BUS_ADDRESS="${DBUS_SESSION_BUS_ADDRESS:?}" \
PIPEWIRE_REMOTE="${PIPEWIRE_REMOTE:?}" \
pipewire-pulse -c "${OMAMIX_PRIVATE_ROOT:?}/config/pipewire/omamix-private-pulse.conf" \
  > "${OMAMIX_PRIVATE_ROOT:?}/log/pulse.log" 2>&1 &
echo $! > "${OMAMIX_PRIVATE_ROOT:?}/run/pulse.pid"
for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
  if [ -S "${XDG_RUNTIME_DIR:?}/pulse/omamix-private-pulse" ]; then
    break
  fi
  sleep 0.2
done
if [ ! -S "${XDG_RUNTIME_DIR:?}/pulse/omamix-private-pulse" ]; then
  echo "private pulse socket was not created" >&2
  exit 1
fi
if [ -S "${XDG_RUNTIME_DIR:?}/pipewire-0" ]; then
  echo "private runtime created pipewire-0" >&2
  exit 1
fi
ls -l "${XDG_RUNTIME_DIR:?}" "${XDG_RUNTIME_DIR:?}/pulse"
```

```sh
XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:?}" \
HOME="${HOME:?}" \
DBUS_SESSION_BUS_ADDRESS="${DBUS_SESSION_BUS_ADDRESS:?}" \
PIPEWIRE_REMOTE="${PIPEWIRE_REMOTE:?}" \
cargo run -p omamix --features pipewire -- --headless --mix-mic --tone --duration 40 \
  > "${OMAMIX_PRIVATE_ROOT:?}/log/omamix.log" 2>&1 &
echo $! > "${OMAMIX_PRIVATE_ROOT:?}/run/omamix.pid"
for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30; do
  if grep -q 'Mix Mic node id' "${OMAMIX_PRIVATE_ROOT:?}/log/omamix.log" 2>/dev/null; then
    break
  fi
  sleep 0.2
done
if ! grep -q 'Mix Mic node id' "${OMAMIX_PRIVATE_ROOT:?}/log/omamix.log"; then
  echo "Mix Mic was not ready" >&2
  exit 1
fi
```

`--duration SECS` exits on its own. `--tone` writes a 440 Hz sine on the mix engine, which is linked into Mix Mic. Mix Mic configures its own adapter ports at creation (`adapter.auto-port-config` and `node.param.PortConfig` on that node, with monitor outputs), so the ports exist without a session manager. WirePlumber, when it is present, links a recorder that names the source. Without a session manager the ports are still there; the link to `capture_FL` / `capture_FR` has to be created by something else. The recorder stream also stays at PortConfig mode none until that stream gets its own `node.param.PortConfig` (direction Input, mode dsp, with the ports the link will use). `pw-record --target` sets a target name and does not set that PortConfig, and it does not create the link by itself. Its remote flag is `--remote` (`-R`).

```sh
XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:?}" \
HOME="${HOME:?}" \
PIPEWIRE_REMOTE="${XDG_RUNTIME_DIR:?}/omamix-private-0" \
timeout 4 pw-record --remote "${XDG_RUNTIME_DIR:?}/omamix-private-0" \
  --media-category Capture --target omamix.mix-mic \
  "${OMAMIX_PRIVATE_ROOT:?}/out/pw-record.wav"
```

```sh
XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:?}" \
HOME="${HOME:?}" \
pw-link -r "${XDG_RUNTIME_DIR:?}/omamix-private-0" \
  omamix.mix-mic:capture_FL pw-record:input_FL
XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:?}" \
HOME="${HOME:?}" \
pw-link -r "${XDG_RUNTIME_DIR:?}/omamix-private-0" \
  omamix.mix-mic:capture_FR pw-record:input_FR
```

`parecord` takes `--device=omamix.mix-mic` and does not accept `--target`. Its `-r` flag means record, not a PipeWire remote. Point it at the private pulse socket with `PULSE_SERVER` and `--server`.

```sh
XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:?}" \
HOME="${HOME:?}" \
PULSE_SERVER="unix:${XDG_RUNTIME_DIR:?}/pulse/omamix-private-pulse" \
timeout 4 parecord --server="unix:${XDG_RUNTIME_DIR:?}/pulse/omamix-private-pulse" \
  --device=omamix.mix-mic "${OMAMIX_PRIVATE_ROOT:?}/out/parecord.wav"
```

```sh
XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:?}" \
HOME="${HOME:?}" \
PULSE_SERVER="unix:${XDG_RUNTIME_DIR:?}/pulse/omamix-private-pulse" \
pactl --server="unix:${XDG_RUNTIME_DIR:?}/pulse/omamix-private-pulse" info
XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:?}" \
HOME="${HOME:?}" \
pw-dump -N -r "${XDG_RUNTIME_DIR:?}/omamix-private-0" > "${OMAMIX_PRIVATE_ROOT:?}/out/pw-dump.json"
```

A bare `pw_filter` is not a Pulse source, and a recorder aimed at the filter name does not attach to it. The adapter is what makes the Mix Mic target work. It leaves the default source and every other device's volume alone. omamix reads `default.audio.source` and `default.audio.sink` from the PipeWire `default` metadata object and logs when either name changes after the first read. If that source name is `omamix.mix-mic`, including when the name is already stored at startup, it destroys the Mix Mic node right away. A stored choice of Mix Mic does this on every start. Pick another source, then start omamix again. If the source key is never read, Mix Mic is not published.

**Do not run the next command against the session that owns the speakers.** It changes `default.audio.source` on whichever daemon `-r` names. `-r` is the private socket from the setup block. After Mix Mic is in that private graph, point the private default at it:

```sh
XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:?}" \
HOME="${HOME:?}" \
pw-metadata -r "${XDG_RUNTIME_DIR:?}/omamix-private-0" \
  -n default 0 default.audio.source '{ "name": "omamix.mix-mic" }'
```

omamix logs the warning and removes the Mix Mic node. The same command before `omamix --headless --mix-mic` is the stored-at-start case: the warning is logged and Mix Mic is not left published. `default.configured` stays absent. Read the private metadata with:

```sh
XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:?}" \
HOME="${HOME:?}" \
pw-metadata -r "${XDG_RUNTIME_DIR:?}/omamix-private-0" -n default
```

Publish Mix Mic while `private-source` is the default, then destroy that other private source by id on the private socket:

```sh
XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:?}" \
HOME="${HOME:?}" \
PRIVATE_SOURCE_ID=$(pw-dump -N -r "${XDG_RUNTIME_DIR:?}/omamix-private-0" | python3 -c 'import json,sys
data=json.load(sys.stdin)
for obj in data:
    props=(obj.get("info") or {}).get("props") or {}
    if props.get("node.name")=="private-source":
        print(obj["id"])
        break')
: "${PRIVATE_SOURCE_ID:?}"
XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:?}" \
HOME="${HOME:?}" \
pw-cli -r "${XDG_RUNTIME_DIR:?}/omamix-private-0" destroy "${PRIVATE_SOURCE_ID:?}"
```

The pass condition is the same warning in the omamix log, no Mix Mic node left on the private daemon, and no `default.configured` key in the private metadata. This document does not record that result.

When the checks are done, the setup block's `EXIT` trap kills the private daemons, deletes the private root, and removes a tmux session named `omamix-priv` if one is still up. Run the live-session read again with the real runtime and the real home, then compare the three mtimes and the hash of those three trees with the setup output:

```sh
stat -c '%n %Y' \
  "${OMAMIX_REAL_HOME:?}/.config/wireplumber" \
  "${OMAMIX_REAL_HOME:?}/.config/pipewire" \
  "${OMAMIX_REAL_HOME:?}/.local/state/wireplumber"
find "${OMAMIX_REAL_HOME:?}/.config/wireplumber" "${OMAMIX_REAL_HOME:?}/.config/pipewire" "${OMAMIX_REAL_HOME:?}/.local/state/wireplumber" -printf '%p %T@ %s\n' | sort > /tmp/omamix-config-after.txt
sha256sum /tmp/omamix-config-after.txt
cmp /tmp/omamix-config-before.txt /tmp/omamix-config-after.txt
env -u PIPEWIRE_REMOTE \
  XDG_RUNTIME_DIR="${OMAMIX_REAL_RUNTIME:?}" \
  HOME="${OMAMIX_REAL_HOME:?}" \
  wpctl status
```

## Unplug test

Tim runs this once, on the login session, after the private procedure has exited. It is read only. It does not set a default, a volume, or a config file. `scripts/unplug-watch.sh` refuses to start when `PIPEWIRE_REMOTE` is set. Pass: the default source name and the default sink name in the last sample match the first sample, and the mtimes of `~/.config/wireplumber`, `~/.config/pipewire`, and `~/.local/state/wireplumber` are unchanged.

```sh
env -u PIPEWIRE_REMOTE \
  XDG_RUNTIME_DIR="${OMAMIX_REAL_RUNTIME:?}" \
  HOME="${OMAMIX_REAL_HOME:?}" \
  scripts/unplug-watch.sh /tmp/omamix-unplug-watch.log
```

Leave that running. Unplug the X2u, wait until the log has a new sample, plug it back in, wait for another sample, then stop the script with Ctrl-C. Compare the first and last `default-source` and `default-sink` lines.

## Definition of done (v1)

Run these on the laptop after the engine and the UI exist. Record the SHA.

- [ ] **Strips.** Add the Shure X2u, an mpv OST, and a Chromium tab. Set levels. Master limiter on, ceiling −1 dBTP.
- [ ] **Chromium getUserMedia.** On a WebRTC mic test page, select **Mix Mic**. The recording contains voice and music at the balance set on the strips. No silence, no doubled music.
- [ ] **Firefox getUserMedia.** The same WebRTC check in Firefox, with **Mix Mic** selected. Voice and music at the set balance. No silence, no doubled music.
- [ ] **gpu-screen-recorder default source.** Tim sets the default source to Mix Mic, records, then restores the previous source. Omarchy screen recording via gpu-screen-recorder captures voice and music. UNVERIFIED: this session did not re-run that test. Tim reported that this case failed for `module-remap-source` (silence until the source was named explicitly).
- [ ] **OBS.** OBS input set to Mix Mic captures the same balance. No silence, no doubling.
- [ ] **Loudness.** The master momentary/short-term LUFS and peak agree with `ffmpeg -i <file> -af ebur128 -f null -` within about 1 dB.
- [ ] **Clean quit.** With `PIPEWIRE_REMOTE` unset, `XDG_RUNTIME_DIR` set to the login runtime, and `HOME` set to the login home, dump nodes and links, quit omamix, and dump again. The diff shows no nodes or links left behind. The default sink and default source are unchanged unless the gsr test above changed the source and the restore step has already run. That dump does not call `pw-metadata`.
- [ ] **Wheel.** Scroll while the pointer is on a fader that has been clicked: wheel-up raises it, and each mouse notch moves it 0.5 dB. A trackpad swipe accumulates and does not jump many dB per event. Scroll anywhere else, including after clicking a fader and moving the pointer away: that fader stays put. The click that focuses a fader does not jump its value. One more notch below −60 dB shows `-inf dB`. `0.0 dB` does not render as `-0.0 dB`.
- [ ] **Re-attach.** Save a scene that includes mpv. Quit mpv, start it again, confirm the strip relinks by `application.name` / binary without a new node id being stored.
- [ ] **Review.** Reviewers and Smoke QA sign off on that same SHA before a merge link goes to Tim.

## Out of scope for the scaffold

No PipeWire nodes are created by the tests. Do not point `pw-record`, `pw-link`, or the omamix binary at the default source from CI. The 5 s Shure X2u capture used to write [AUDIO-BASELINE.md](AUDIO-BASELINE.md) was a one-off read of an existing node, targeted by name, and the wav was deleted. `pw-record` briefly linked to that capture node; the link was gone when the process exited.
