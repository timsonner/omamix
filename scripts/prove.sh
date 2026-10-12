#!/bin/bash
# Private PipeWire procedure for docs/TESTING.md.
# Execute this file. Do not source it.
#
# No verbose daemon flags. Stdout and stderr go to files under the private
# root. Daemons are children of this script, not service-manager units, and
# nothing is written to the journal. Pidfiles live under the private root and
# are never printed.
# trap cleanup EXIT INT TERM tears the daemons down on success, failure, and
# Ctrl-C. After cleanup, pgrep -f reports a count, not pids.

if [ -z "${BASH_VERSION:-}" ]; then
  echo "scripts/prove.sh needs bash" >&2
  exit 1
fi

if [ "${BASH_SOURCE[0]}" != "$0" ]; then
  echo "execute scripts/prove.sh; do not source it" >&2
  return 1
fi

set -u
set -o pipefail
set -m

OMAMIX_REPO=$(cd "$(dirname "$0")/.." && pwd)
cd "$OMAMIX_REPO"

OMAMIX_REAL_HOME=${HOME:?}
OMAMIX_REAL_RUNTIME=${XDG_RUNTIME_DIR:?}
OMAMIX_CLEANED=0
OMAMIX_PRIVATE_ROOT=

die() {
  echo "$1" >&2
  cleanup
  trap - EXIT INT TERM
  exit 1
}

# Kill every daemon this script started, remove the private root, then count
# leftover processes. The count is the only process fact this function prints.
cleanup() {
  if [ "$OMAMIX_CLEANED" = 1 ]; then
    return 0
  fi
  OMAMIX_CLEANED=1

  root=$OMAMIX_PRIVATE_ROOT
  # Pidfiles record the process. A second walk catches a child whose command
  # line or environment still contains the private root. The shell running
  # this script is skipped. Pids are not printed.
  signal_private() {
    sig=$1
    [ -n "$root" ] || return 0
    if [ -d "$root/run" ]; then
      for pidfile in "$root"/run/*.pid; do
        [ -f "$pidfile" ] || continue
        pid=$(cat "$pidfile" 2>/dev/null || true)
        if [ -n "${pid:-}" ]; then
          kill "-$sig" -- "-$pid" 2>/dev/null || kill "-$sig" "$pid" 2>/dev/null || true
        fi
      done
    fi
    for cmdf in /proc/[0-9]*/cmdline; do
      pid=${cmdf#/proc/}
      pid=${pid%/cmdline}
      if [ "$pid" = "$$" ]; then
        continue
      fi
      if grep -q -a -F -z "$root" "$cmdf" 2>/dev/null \
        || grep -q -a -F -z "$root" "/proc/$pid/environ" 2>/dev/null; then
        kill "-$sig" "$pid" 2>/dev/null || true
      fi
    done
  }
  if [ -n "$root" ]; then
    signal_private TERM
    sleep 0.3
    signal_private KILL
    wait 2>/dev/null || true
    mkdir -p /tmp/omamix-private-log /tmp/omamix-private-out
    if [ -d "$root/log" ]; then
      cp -a "$root/log/." /tmp/omamix-private-log/ 2>/dev/null || true
    fi
    if [ -d "$root/out" ]; then
      cp -a "$root/out/." /tmp/omamix-private-out/ 2>/dev/null || true
    fi
  fi

  unset XDG_RUNTIME_DIR XDG_CONFIG_HOME XDG_STATE_HOME XDG_DATA_HOME \
    PIPEWIRE_REMOTE PIPEWIRE_RUNTIME_DIR PULSE_RUNTIME_PATH PULSE_SERVER \
    DBUS_SESSION_BUS_ADDRESS || true
  export HOME=$OMAMIX_REAL_HOME
  export XDG_RUNTIME_DIR=$OMAMIX_REAL_RUNTIME

  if [ -n "$root" ]; then
    case "$root" in
      /tmp/omamix-private.*)
        rm -rf -- "$root"
        ;;
      *)
        echo "refusing to remove an unexpected private root" >&2
        ;;
    esac
  fi

  if [ -z "$root" ]; then
    return 0
  fi

  # The bracket keeps pgrep's own command line from matching the path.
  escaped=$(printf '%s' "$root" | sed 's/[].[*^$()+?{|\\]/\\&/g')
  pattern="[${escaped:0:1}]${escaped:1}"
  cmd_count=$(pgrep -f -c "$pattern" 2>/dev/null || true)
  cmd_count=${cmd_count:-0}

  env_count=0
  for envf in /proc/[0-9]*/environ; do
    if grep -q -a -F -z "$root" "$envf" 2>/dev/null; then
      env_count=$((env_count + 1))
    fi
  done

  echo "private-root command lines: $cmd_count"
  echo "private-root environments: $env_count"
  if [ "$cmd_count" != 0 ] || [ "$env_count" != 0 ]; then
    echo "a private-root process is still running" >&2
    return 1
  fi
}

trap 'cleanup; trap - EXIT INT TERM; exit 130' INT TERM
trap cleanup EXIT

echo "building target/debug/omamix"
cargo build -p omamix --features pipewire --offline --manifest-path "$OMAMIX_REPO/Cargo.toml" \
  || die "cargo build failed"
if [ ! -x "$OMAMIX_REPO/target/debug/omamix" ]; then
  die "prebuilt omamix binary is missing"
fi

env -u PIPEWIRE_REMOTE -u PIPEWIRE_RUNTIME_DIR -u PULSE_RUNTIME_PATH \
  XDG_RUNTIME_DIR="$OMAMIX_REAL_RUNTIME" \
  HOME="$OMAMIX_REAL_HOME" \
  wpctl status > /tmp/omamix-wpctl-before.txt
{
  stat -c '%n %Y' \
    "$OMAMIX_REAL_HOME/.config/wireplumber" \
    "$OMAMIX_REAL_HOME/.config/pipewire" \
    "$OMAMIX_REAL_HOME/.local/state/wireplumber"
  find \
    "$OMAMIX_REAL_HOME/.config/wireplumber" \
    "$OMAMIX_REAL_HOME/.config/pipewire" \
    "$OMAMIX_REAL_HOME/.local/state/wireplumber" \
    -printf '%p %T@\n' 2>/dev/null | sort
} > /tmp/omamix-stat-before.txt
echo "stat before:"
stat -c '%n %Y' \
  "$OMAMIX_REAL_HOME/.config/wireplumber" \
  "$OMAMIX_REAL_HOME/.config/pipewire" \
  "$OMAMIX_REAL_HOME/.local/state/wireplumber"

OMAMIX_PRIVATE_ROOT=$(mktemp -d /tmp/omamix-private.XXXXXX)
case "$OMAMIX_PRIVATE_ROOT" in
  /tmp/omamix-private.*) ;;
  *) die "private root must be a fresh /tmp/omamix-private directory" ;;
esac
case "$OMAMIX_PRIVATE_ROOT" in
  /tmp/omamix-fixround | /tmp/omamix-fixround/*)
    die "refusing /tmp/omamix-fixround"
    ;;
esac
if [ "$OMAMIX_PRIVATE_ROOT" = "$OMAMIX_REAL_HOME" ]; then
  die "private root must not be the real home"
fi

install -d \
  "$OMAMIX_PRIVATE_ROOT/runtime" \
  "$OMAMIX_PRIVATE_ROOT/config" \
  "$OMAMIX_PRIVATE_ROOT/state" \
  "$OMAMIX_PRIVATE_ROOT/data" \
  "$OMAMIX_PRIVATE_ROOT/home" \
  "$OMAMIX_PRIVATE_ROOT/log" \
  "$OMAMIX_PRIVATE_ROOT/out" \
  "$OMAMIX_PRIVATE_ROOT/run"

# Exports and the runtime guard come before any config is copied.
export XDG_RUNTIME_DIR="$OMAMIX_PRIVATE_ROOT/runtime"
export XDG_CONFIG_HOME="$OMAMIX_PRIVATE_ROOT/config"
export XDG_STATE_HOME="$OMAMIX_PRIVATE_ROOT/state"
export XDG_DATA_HOME="$OMAMIX_PRIVATE_ROOT/data"
export HOME="$OMAMIX_PRIVATE_ROOT/home"
unset PIPEWIRE_REMOTE PIPEWIRE_RUNTIME_DIR PULSE_RUNTIME_PATH || true

case "${XDG_RUNTIME_DIR:?}" in
  /run/user | /run/user/*) die "refusing a runtime under /run/user" ;;
esac
if [ "${XDG_RUNTIME_DIR:?}" = "${OMAMIX_REAL_RUNTIME:?}" ]; then
  die "private runtime must not be the login runtime"
fi

install -d \
  "$XDG_CONFIG_HOME/pipewire" \
  "$XDG_CONFIG_HOME/wireplumber/wireplumber.conf.d" \
  "$XDG_RUNTIME_DIR/pulse"

cp "$OMAMIX_REPO/docs/omamix-private.conf" \
  "$XDG_CONFIG_HOME/wireplumber/wireplumber.conf.d/omamix-private.conf" \
  || die "could not copy the wireplumber profile"
cp "$OMAMIX_REPO/docs/omamix-private-pulse.conf" \
  "$XDG_CONFIG_HOME/pipewire/omamix-private-pulse.conf" \
  || die "could not copy the pulse config"

dbus_socket=$XDG_RUNTIME_DIR/omamix-private-bus
case "$dbus_socket" in
  /run/user | /run/user/*) die "refusing a session bus under /run/user" ;;
esac

start_bg() {
  name=$1
  shift
  # Job control makes this process its own group, so cleanup can signal the
  # group from the pidfile. The pid is not printed.
  "$@" >"$OMAMIX_PRIVATE_ROOT/log/${name}.log" 2>&1 </dev/null &
  pid=$!
  printf '%s\n' "$pid" >"$OMAMIX_PRIVATE_ROOT/run/${name}.pid"
  disown "$pid" 2>/dev/null || true
}

start_bg dbus \
  dbus-daemon --session --nofork --nopidfile --address "unix:path=${dbus_socket}"
export DBUS_SESSION_BUS_ADDRESS="unix:path=${dbus_socket}"

for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
  if [ -S "$dbus_socket" ]; then
    break
  fi
  sleep 0.1
done
if [ ! -S "$dbus_socket" ]; then
  die "private dbus socket did not appear"
fi

start_bg pipewire \
  pipewire -c "$OMAMIX_REPO/docs/private-pipewire.conf"

for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 \
         21 22 23 24 25 26 27 28 29 30 31 32 33 34 35 36 37 38 39 40 \
         41 42 43 44 45 46 47 48 49 50; do
  if [ -S "${XDG_RUNTIME_DIR:?}/omamix-private-0" ]; then
    break
  fi
  sleep 0.1
done
if [ ! -S "${XDG_RUNTIME_DIR:?}/omamix-private-0" ]; then
  die "private socket did not appear"
fi
if [ -S "${XDG_RUNTIME_DIR:?}/pipewire-0" ]; then
  die "refusing pipewire-0 in the private runtime"
fi

start_bg wireplumber \
  env PIPEWIRE_REMOTE="${XDG_RUNTIME_DIR}/omamix-private-0" \
  wireplumber -p omamix-private

for _ in 1 2 3 4 5 6 7 8 9 10; do
  sleep 0.2
done

start_bg pulse \
  env PIPEWIRE_REMOTE="${XDG_RUNTIME_DIR}/omamix-private-0" \
  pipewire-pulse -c "$XDG_CONFIG_HOME/pipewire/omamix-private-pulse.conf"

for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 \
         21 22 23 24 25 26 27 28 29 30 31 32 33 34 35 36 37 38 39 40 \
         41 42 43 44 45 46 47 48 49 50; do
  if [ -S "${XDG_RUNTIME_DIR:?}/pulse/omamix-private-pulse" ]; then
    break
  fi
  sleep 0.1
done
if [ ! -S "${XDG_RUNTIME_DIR:?}/pulse/omamix-private-pulse" ]; then
  die "private pulse socket did not appear"
fi

start_bg omamix \
  env PIPEWIRE_REMOTE="${XDG_RUNTIME_DIR}/omamix-private-0" \
  stdbuf -oL -eL "$OMAMIX_REPO/target/debug/omamix" \
  --headless --mix-mic --tone --duration 180

ready=0
for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 \
         21 22 23 24 25 26 27 28 29 30 31 32 33 34 35 36 37 38 39 40 \
         41 42 43 44 45 46 47 48 49 50; do
  if grep -q 'Mix Mic node id' "$OMAMIX_PRIVATE_ROOT/log/omamix.log"; then
    ready=1
    break
  fi
  sleep 0.2
done
if [ "$ready" != 1 ]; then
  die "Mix Mic did not become ready"
fi
echo "Mix Mic ready"

socket="${XDG_RUNTIME_DIR:?}/omamix-private-0"

# Background the recorder first. Link only after its node exists, then wait
# before stopping it, so the tone is in the file and not only in a header.
record_tone() {
  kind=$1
  wav="$OMAMIX_PRIVATE_ROOT/out/${kind}.wav"
  rm -f -- "$wav"
  if [ "$kind" = "pw-record" ]; then
    start_bg pw-record \
      pw-record --target 0 \
      -R "$socket" \
      --rate 48000 --channels 2 --format s16 \
      "$wav"
    binary=pw-record
  else
    start_bg parecord \
      parecord \
      -s "unix:${XDG_RUNTIME_DIR:?}/pulse/omamix-private-pulse" \
      --device=omamix.mix-mic \
      --rate=48000 --channels=2 --format=s16le \
      --file-format=wav \
      "$wav"
    binary=parecord
  fi

  python3 - "$kind" "$binary" "$socket" "$wav" "$OMAMIX_PRIVATE_ROOT" <<'PY' || return 1
import json, math, os, struct, subprocess, sys, time, wave

kind, binary, socket, wav, root = sys.argv[1:]

def dump():
    raw = subprocess.check_output(
        ["pw-dump", "-N", "-r", socket], text=True
    )
    return json.loads(raw)

def nodes(data):
    return [o for o in data if o.get("type") == "PipeWire:Interface:Node"]

def ports(data):
    return [o for o in data if o.get("type") == "PipeWire:Interface:Port"]

def props(obj):
    return (obj.get("info") or {}).get("props") or {}

def find_recorder(data):
    for obj in nodes(data):
        p = props(obj)
        b = p.get("application.process.binary") or ""
        n = p.get("application.name") or ""
        base = os.path.basename(b)
        if base == binary or n == binary or b.endswith("/" + binary):
            return obj["id"]
    return None

def port_id(data, node_id, name):
    for obj in ports(data):
        p = props(obj)
        if p.get("node.id") != node_id and str(p.get("node.id")) != str(node_id):
            continue
        if p.get("port.name") == name or p.get("audio.channel") == name:
            return obj["id"]
    return None

node_id = None
for _ in range(50):
    node_id = find_recorder(dump())
    if node_id is not None:
        break
    time.sleep(0.1)
if node_id is None:
    print(f"{kind}: recorder node did not appear", file=sys.stderr)
    sys.exit(1)

port_config = json.dumps({
    "direction": "Input",
    "mode": "dsp",
    "monitor": False,
    "control": False,
    "format": {
        "mediaType": "audio",
        "mediaSubtype": "raw",
        "format": "S16",
        "rate": 48000,
        "channels": 2,
        "position": ["FL", "FR"],
    },
})
subprocess.run(
    ["pw-cli", "-r", socket, "set-param", str(node_id), "PortConfig", port_config],
    check=False,
    stdout=subprocess.DEVNULL,
)

in_fl = in_fr = None
for _ in range(50):
    data = dump()
    in_fl = port_id(data, node_id, "input_FL") or port_id(data, node_id, "FL")
    in_fr = port_id(data, node_id, "input_FR") or port_id(data, node_id, "FR")
    # A mono capture port is still a live input.
    if in_fl is None:
        for obj in ports(data):
            p = props(obj)
            if str(p.get("node.id")) == str(node_id) and p.get("port.direction") == "in":
                in_fl = obj["id"]
                break
    if in_fl is not None:
        break
    time.sleep(0.1)
if in_fl is None:
    print(f"{kind}: recorder input port did not appear", file=sys.stderr)
    sys.exit(1)

data = dump()
mix = None
for obj in nodes(data):
    if props(obj).get("node.name") == "omamix.mix-mic":
        mix = obj["id"]
        break
if mix is None:
    print(f"{kind}: omamix.mix-mic is gone", file=sys.stderr)
    sys.exit(1)
out_fl = port_id(data, mix, "capture_FL")
out_fr = port_id(data, mix, "capture_FR")
if out_fl is None or out_fr is None:
    print(f"{kind}: mix mic capture ports are missing", file=sys.stderr)
    sys.exit(1)

def link(src, dst):
    result = subprocess.run(
        ["pw-link", "-r", socket, str(src), str(dst)],
        text=True,
        capture_output=True,
    )
    if result.returncode != 0:
        text = (result.stderr or "") + (result.stdout or "")
        if "File exists" not in text and "exists" not in text.lower():
            print(f"{kind}: link failed", file=sys.stderr)
            sys.exit(1)

link(out_fl, in_fl)
if in_fr is not None:
    link(out_fr, in_fr)

time.sleep(4)

pidfile = os.path.join(root, "run", f"{kind}.pid")
pid = open(pidfile).read().strip()
subprocess.run(["kill", "-TERM", pid], check=False)
for _ in range(30):
    try:
        os.kill(int(pid), 0)
    except OSError:
        break
    time.sleep(0.1)
else:
    subprocess.run(["kill", "-KILL", pid], check=False)

if not os.path.isfile(wav) or os.path.getsize(wav) <= 44:
    print(f"{kind}: file is missing or header-only", file=sys.stderr)
    sys.exit(1)

w = wave.open(wav, "rb")
frames = w.getnframes()
rate = w.getframerate()
channels = w.getnchannels()
width = w.getsampwidth()
raw = w.readframes(frames)
w.close()
if frames < rate // 2:
    print(f"{kind}: file is too short", file=sys.stderr)
    sys.exit(1)

samples = []
if width == 2:
    count = len(raw) // 2
    values = struct.unpack("<" + "h" * count, raw[: count * 2])
    scale = 32768.0
elif width == 4:
    count = len(raw) // 4
    values = struct.unpack("<" + "i" * count, raw[: count * 4])
    scale = 2147483648.0
else:
    print(f"{kind}: unsupported sample width {width}", file=sys.stderr)
    sys.exit(1)
for i in range(0, len(values), channels):
    samples.append(values[i] / scale)

def goertzel(freq):
    n = len(samples)
    k = int(0.5 + (n * freq) / rate)
    w0 = 2 * math.pi * k / n
    coeff = 2 * math.cos(w0)
    s1 = s2 = 0.0
    for x in samples:
        s0 = x + coeff * s1 - s2
        s2 = s1
        s1 = s0
    return s1 * s1 + s2 * s2 - coeff * s1 * s2

peak = 0.0
for x in samples:
    a = abs(x)
    if a > peak:
        peak = a
powers = {freq: goertzel(freq) for freq in (100, 220, 440, 880, 1000, 2000)}
best = max(powers, key=powers.get)
print(
    f"{kind} bytes={os.path.getsize(wav)} frames={frames} rate={rate} "
    f"peak={peak:.4f} best={best}"
)
if peak < 0.02 or best != 440:
    sys.exit(1)
PY
}

record_tone pw-record || die "pw-record did not capture 440 Hz"
record_tone parecord || die "parecord did not capture 440 Hz"

XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:?}" \
  HOME="${HOME:?}" \
  pactl --server "unix:${XDG_RUNTIME_DIR:?}/pulse/omamix-private-pulse" info \
  > "$OMAMIX_PRIVATE_ROOT/log/pactl-info.txt"

XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:?}" \
  HOME="${HOME:?}" \
  pw-dump -N -r "${XDG_RUNTIME_DIR:?}/omamix-private-0" \
  > "$OMAMIX_PRIVATE_ROOT/out/private-dump.json"

meta=$(
  XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:?}" \
    HOME="${HOME:?}" \
    pw-metadata -r "${XDG_RUNTIME_DIR:?}/omamix-private-0" -n default
)
printf '%s\n' "$meta" | grep -q 'default.audio.sink' || die "default.audio.sink missing"
printf '%s\n' "$meta" | grep -q 'default.audio.source' || die "default.audio.source missing"
printf '%s\n' "$meta" | grep -q 'default.configured' && die "default.configured was present before the warning check"

# After the captures: point the private default at Mix Mic. omamix removes
# that node and logs the warning. This stays on the private socket.
XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:?}" \
  HOME="${HOME:?}" \
  pw-metadata -r "${XDG_RUNTIME_DIR:?}/omamix-private-0" \
  -n default 0 default.audio.source '{ "name": "omamix.mix-mic" }' \
  >"$OMAMIX_PRIVATE_ROOT/log/metadata-set.txt" 2>&1

warned=0
for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
  if grep -q 'Mix Mic is the default source' "$OMAMIX_PRIVATE_ROOT/log/omamix.log"; then
    warned=1
    break
  fi
  sleep 0.1
done
if [ "$warned" != 1 ]; then
  die "Mix Mic default-source warning was not logged"
fi
echo "Mix Mic default-source warning logged"

python3 - "${XDG_RUNTIME_DIR:?}/omamix-private-0" "$OMAMIX_PRIVATE_ROOT/log/omamix.log" <<'PY' || die "Mix Mic was still published after the warning"
import json, subprocess, sys
socket = sys.argv[1]
raw = subprocess.check_output(["pw-dump", "-N", "-r", socket], text=True)
source_id = None
for obj in json.loads(raw):
    if obj.get("type") != "PipeWire:Interface:Node":
        continue
    props = (obj.get("info") or {}).get("props") or {}
    if props.get("node.name") == "omamix.mix-mic":
        sys.exit(1)
    if props.get("node.name") == "private-source":
        source_id = str(obj["id"])
meta = subprocess.check_output(
    ["pw-metadata", "-r", socket, "-n", "default"], text=True
)
if "default.configured" in meta:
    sys.exit(1)
if source_id is None:
    sys.exit(1)
subprocess.check_call(
    ["pw-cli", "-r", socket, "destroy", source_id],
    stdout=subprocess.DEVNULL,
)
print("private-source removed")
PY

cleanup || die "private-root processes remained after cleanup"
trap - EXIT INT TERM

env -u PIPEWIRE_REMOTE -u PIPEWIRE_RUNTIME_DIR -u PULSE_RUNTIME_PATH \
  XDG_RUNTIME_DIR="$OMAMIX_REAL_RUNTIME" \
  HOME="$OMAMIX_REAL_HOME" \
  wpctl status > /tmp/omamix-wpctl-after.txt
{
  stat -c '%n %Y' \
    "$OMAMIX_REAL_HOME/.config/wireplumber" \
    "$OMAMIX_REAL_HOME/.config/pipewire" \
    "$OMAMIX_REAL_HOME/.local/state/wireplumber"
  find \
    "$OMAMIX_REAL_HOME/.config/wireplumber" \
    "$OMAMIX_REAL_HOME/.config/pipewire" \
    "$OMAMIX_REAL_HOME/.local/state/wireplumber" \
    -printf '%p %T@\n' 2>/dev/null | sort
} > /tmp/omamix-stat-after.txt

# Each wpctl status is a new client, so that one line's id and pid change
# every run. Compare the rest of the login session. Do not print the status.
sed -E '/^[[:space:]]*[0-9]+\. wpctl[[:space:]]/d; s/pid:[0-9]+/pid:PID/g' \
  /tmp/omamix-wpctl-before.txt > /tmp/omamix-wpctl-before.norm
sed -E '/^[[:space:]]*[0-9]+\. wpctl[[:space:]]/d; s/pid:[0-9]+/pid:PID/g' \
  /tmp/omamix-wpctl-after.txt > /tmp/omamix-wpctl-after.norm
if ! cmp -s /tmp/omamix-wpctl-before.norm /tmp/omamix-wpctl-after.norm; then
  die "wpctl status changed"
fi
if ! cmp -s /tmp/omamix-stat-before.txt /tmp/omamix-stat-after.txt; then
  echo "stat after:" >&2
  cat /tmp/omamix-stat-after.txt >&2
  die "wireplumber or pipewire config mtime changed"
fi
echo "wpctl status unchanged"
echo "stat after:"
stat -c '%n %Y' \
  "$OMAMIX_REAL_HOME/.config/wireplumber" \
  "$OMAMIX_REAL_HOME/.config/pipewire" \
  "$OMAMIX_REAL_HOME/.local/state/wireplumber"
echo "private procedure passed"
