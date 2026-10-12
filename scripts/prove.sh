#!/bin/bash
# Private PipeWire procedure for docs/TESTING.md.
# Execute this file. Do not source it.
#
# No verbose daemon flags. Stdout and stderr go to files under the per-run
# scratch directory. Daemons are children of this script, not service-manager units.
# PIPEWIRE_LOG_SYSTEMD=false is set before those daemons start, and
# PIPEWIRE_DEBUG and WIREPLUMBER_DEBUG are unset, so daemon log lines go to
# the private log files. That does not cover rtkit-daemon. It is a system
# service, and it writes the journal when a process asks it for realtime.
# The private pipewire and pipewire-pulse copies set rtkit.enabled = false.
# DISABLE_RTKIT is set for the private processes so wireplumber and the
# clients, which load libpipewire-module-rt from the system config, do not
# ask rtkit either. Pidfiles live under the private root and are never printed.
# trap cleanup EXIT INT TERM HUP tears the daemons down on success, failure,
# Ctrl-C, SIGTERM, and SIGHUP. Monitor mode is not global. It is on only
# while start_bg spawns a daemon, so that daemon gets its own process group.
# A terminal Ctrl-C stays with this script and exits 130. cleanup removes the
# per-run scratch directory and the private root. A second signal during
# cleanup still removes the private root when that directory is still there.
# cleanup returns early only after the root is gone. wpctl and stat snapshots
# stay in shell variables, so a signal does not leave a status file behind.
# After cleanup, pgrep -f reports a count, not pids.
# Exit 0 is a clean pass. Exit 3 is PASSED WITH GAPS. Exit 1 is a failure.
# SIGINT exits 130. SIGTERM exits 143. SIGHUP exits 129.

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

OMAMIX_REPO=$(cd "$(dirname "$0")/.." && pwd)
cd "$OMAMIX_REPO" || exit 1

OMAMIX_REAL_HOME=${HOME:?}
OMAMIX_REAL_RUNTIME=${XDG_RUNTIME_DIR:?}
OMAMIX_CLEANED=0
OMAMIX_LOGS_KEPT=0
OMAMIX_PRIVATE_ROOT=
OMAMIX_SCRATCH=
OMAMIX_WPCTL_BEFORE=0
OMAMIX_WPCTL_BEFORE_NORM=
OMAMIX_WPCTL_AFTER_NORM=
OMAMIX_STAT_BEFORE=
OMAMIX_STAT_AFTER=
OMAMIX_WP_STARTED=0
OMAMIX_GAPS=()
OMAMIX_HAD_DISABLE_RTKIT=0
OMAMIX_OLD_DISABLE_RTKIT=

# Only a fresh mktemp directory under /tmp is removed. /tmp/omamix-fixround
# is refused and is never a scratch path.
drop_scratch() {
  dir=$OMAMIX_SCRATCH
  [ -n "$dir" ] || return 0
  case "$dir" in
    /tmp/omamix-fixround | /tmp/omamix-fixround/*)
      echo "refusing to remove /tmp/omamix-fixround" >&2
      return 1
      ;;
    /tmp/omamix-scratch.*)
      rm -rf -- "$dir"
      OMAMIX_SCRATCH=
      ;;
    *)
      echo "refusing to remove an unexpected scratch dir" >&2
      return 1
      ;;
  esac
}

# Opt-in only. The kept directory is a fresh mktemp name, not a fixed path.
keep_logs_if_requested() {
  [ "${OMAMIX_KEEP_LOGS:-}" = 1 ] || return 0
  # A re-entered cleanup must not mint a second keep directory.
  [ "${OMAMIX_LOGS_KEPT:-}" = 1 ] && return 0
  [ -n "$OMAMIX_SCRATCH" ] || return 0
  [ -d "$OMAMIX_SCRATCH" ] || return 0
  OMAMIX_LOGS_KEPT=1
  keep=$(mktemp -d /tmp/omamix-keep.XXXXXX) || return 0
  case "$keep" in
    /tmp/omamix-fixround | /tmp/omamix-fixround/*)
      rm -rf -- "$keep"
      echo "refusing to keep logs under /tmp/omamix-fixround" >&2
      return 0
      ;;
    /tmp/omamix-keep.*) ;;
    *)
      rm -rf -- "$keep"
      echo "refusing to keep logs in an unexpected directory" >&2
      return 0
      ;;
  esac
  if [ -d "$OMAMIX_SCRATCH/log" ]; then
    cp -a "$OMAMIX_SCRATCH/log" "$keep/log" 2>/dev/null || true
  fi
  if [ -d "$OMAMIX_SCRATCH/out" ]; then
    cp -a "$OMAMIX_SCRATCH/out" "$keep/out" 2>/dev/null || true
  fi
  echo "kept private logs: $keep"
}

# A missing config directory is the line "absent" plus its path. That is a
# pass when the after snapshot still says absent. An appearance, a removal,
# or any other listing change fails the comparison.
config_snapshot() {
  dir=$1
  if [ -d "$dir" ]; then
    stat -c '%n %Y' "$dir" || return 1
    find "$dir" -printf '%p %T@\n' | sort
    return
  fi
  if [ -e "$dir" ]; then
    echo "config path is not a directory" >&2
    return 1
  fi
  printf 'absent %s\n' "$dir"
}

collect_stat() {
  config_snapshot "$OMAMIX_REAL_HOME/.config/wireplumber" || return 1
  config_snapshot "$OMAMIX_REAL_HOME/.config/pipewire" || return 1
  config_snapshot "$OMAMIX_REAL_HOME/.local/state/wireplumber" || return 1
}

print_config_mtimes() {
  local dir
  for dir in \
    "$OMAMIX_REAL_HOME/.config/wireplumber" \
    "$OMAMIX_REAL_HOME/.config/pipewire" \
    "$OMAMIX_REAL_HOME/.local/state/wireplumber"
  do
    if [ -d "$dir" ]; then
      stat -c '%n %Y' "$dir" || return 1
    elif [ -e "$dir" ]; then
      echo "config path is not a directory" >&2
      return 1
    else
      printf 'absent %s\n' "$dir"
    fi
  done
}

normalize_wpctl() {
  sed -E '/^[[:space:]]*[0-9]+\. wpctl[[:space:]]/d; s/pid:[0-9]+/pid:PID/g'
}

add_gap() {
  OMAMIX_GAPS+=("$1")
  echo "$1"
}

# The committed templates set rtkit.enabled = false. Re-apply it on the
# private copy if that line is missing. Never edit a file outside the
# private root.
set_rtkit_disabled() {
  conf=$1
  case "$conf" in
    "${OMAMIX_PRIVATE_ROOT:?}"/*) ;;
    *) die "refusing to edit a config outside the private root" ;;
  esac
  if ! grep -q 'rtportal.enabled = false' "$conf"; then
    die "private config is missing the rt module args"
  fi
  if ! grep -q 'rtkit.enabled = false' "$conf"; then
    sed -i 's/rtportal.enabled = false/rtportal.enabled = false rtkit.enabled = false/' "$conf" \
      || die "could not set rtkit.enabled = false"
  fi
  if ! grep -q 'rtkit.enabled = false' "$conf"; then
    die "rtkit.enabled = false was not set"
  fi
}

die() {
  echo "$1" >&2
  cleanup
  trap - EXIT INT TERM HUP
  exit 1
}

# Kill every daemon this script started, remove the private root, then count
# leftover processes. The count is the only process fact this function prints.
# The scratch directory holds logs and recordings. wpctl and stat text stays
# in shell variables. A second call returns only when the private root is
# already gone. If that directory is still there, the second call finishes
# the kill and the rm. It still drops the scratch directory.
cleanup() {
  if [ "$OMAMIX_CLEANED" = 1 ] && [ ! -d "${OMAMIX_PRIVATE_ROOT:-}" ]; then
    drop_scratch
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
      # A process can exit between the glob and the open. Skip it. Do not
      # let grep print ENOENT.
      [ -r "$cmdf" ] || continue
      pid=${cmdf#/proc/}
      pid=${pid%/cmdline}
      if [ "$pid" = "$$" ]; then
        continue
      fi
      if grep -q -a -F -z "$root" "$cmdf" 2>/dev/null \
        || { [ -r "/proc/$pid/environ" ] && grep -q -a -F -z "$root" "/proc/$pid/environ" 2>/dev/null; }; then
        kill "-$sig" "$pid" 2>/dev/null || true
      fi
      if [ -n "$OMAMIX_SCRATCH" ] \
        && { grep -q -a -F -z "$OMAMIX_SCRATCH" "$cmdf" 2>/dev/null \
          || { [ -r "/proc/$pid/environ" ] && grep -q -a -F -z "$OMAMIX_SCRATCH" "/proc/$pid/environ" 2>/dev/null; }; }; then
        kill "-$sig" "$pid" 2>/dev/null || true
      fi
    done
  }
  if [ -n "$root" ]; then
    signal_private TERM
    sleep 0.3
    signal_private KILL
    wait 2>/dev/null || true
    keep_logs_if_requested
  fi

  unset XDG_RUNTIME_DIR XDG_CONFIG_HOME XDG_STATE_HOME XDG_DATA_HOME \
    PIPEWIRE_REMOTE PIPEWIRE_RUNTIME_DIR PULSE_RUNTIME_PATH PULSE_SERVER \
    DBUS_SESSION_BUS_ADDRESS PIPEWIRE_LOG_SYSTEMD \
    PIPEWIRE_DEBUG WIREPLUMBER_DEBUG DISABLE_RTKIT || true
  if [ "$OMAMIX_HAD_DISABLE_RTKIT" = 1 ]; then
    export DISABLE_RTKIT="$OMAMIX_OLD_DISABLE_RTKIT"
  fi
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
  drop_scratch

  if [ -z "$root" ]; then
    return 0
  fi

  # The bracket keeps pgrep's own command line from matching the path.
  # Escape regex metacharacters without a single-quoted sed expression.
  escaped=$root
  escaped=${escaped//\\/\\\\}
  escaped=${escaped//./\\.}
  escaped=${escaped//\*/\\*}
  escaped=${escaped//\[/\\[}
  escaped=${escaped//\]/\\]}
  escaped=${escaped//\^/\\^}
  escaped=${escaped//\$/\\$}
  escaped=${escaped//\(/\\(}
  escaped=${escaped//\)/\\)}
  escaped=${escaped//+/\\+}
  escaped=${escaped//\?/\\?}
  escaped=${escaped//\{/\\\{}
  escaped=${escaped//\|/\\|}
  pattern="[${escaped:0:1}]${escaped:1}"
  cmd_count=$(pgrep -f -c "$pattern" 2>/dev/null || true)
  cmd_count=${cmd_count:-0}

  env_count=0
  for envf in /proc/[0-9]*/environ; do
    [ -r "$envf" ] || continue
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

trap 'cleanup; trap - EXIT INT TERM HUP; exit 130' INT
trap 'cleanup; trap - EXIT INT TERM HUP; exit 143' TERM
trap 'cleanup; trap - EXIT INT TERM HUP; exit 129' HUP
trap cleanup EXIT

OMAMIX_SCRATCH=$(mktemp -d /tmp/omamix-scratch.XXXXXX) || die "could not create the scratch dir"
case "$OMAMIX_SCRATCH" in
  /tmp/omamix-scratch.*) ;;
  *) die "scratch dir must be a fresh /tmp/omamix-scratch directory" ;;
esac
case "$OMAMIX_SCRATCH" in
  /tmp/omamix-fixround | /tmp/omamix-fixround/*)
    die "refusing /tmp/omamix-fixround"
    ;;
esac
OMAMIX_PRIVATE_ROOT=$(mktemp -d /tmp/omamix-private.XXXXXX) || die "could not create the private root"
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
install -d "$OMAMIX_SCRATCH/log" "$OMAMIX_SCRATCH/out"

echo "building target/debug/omamix"
cargo build -p omamix --features pipewire --offline --manifest-path "$OMAMIX_REPO/Cargo.toml" \
  || die "cargo build failed"
if [ ! -x "$OMAMIX_REPO/target/debug/omamix" ]; then
  die "prebuilt omamix binary is missing"
fi

# wpctl status can carry a device serial. Keep it in a variable, never a file,
# so SIGKILL does not leave a snapshot.
wpctl_before_raw=$(env -u PIPEWIRE_REMOTE -u PIPEWIRE_RUNTIME_DIR -u PULSE_RUNTIME_PATH \
  XDG_RUNTIME_DIR="$OMAMIX_REAL_RUNTIME" \
  HOME="$OMAMIX_REAL_HOME" \
  wpctl status) && wpctl_before_ok=1 || wpctl_before_ok=0
if [ "$wpctl_before_ok" = 1 ]; then
  OMAMIX_WPCTL_BEFORE=1
  OMAMIX_WPCTL_BEFORE_NORM=$(printf '%s\n' "$wpctl_before_raw" | normalize_wpctl) \
    || die "could not normalize wpctl before"
fi
unset wpctl_before_raw
OMAMIX_STAT_BEFORE=$(collect_stat) || die "could not stat config trees"
echo "stat before:"
print_config_mtimes || die "could not stat config trees"

install -d \
  "$OMAMIX_PRIVATE_ROOT/runtime" \
  "$OMAMIX_PRIVATE_ROOT/config" \
  "$OMAMIX_PRIVATE_ROOT/state" \
  "$OMAMIX_PRIVATE_ROOT/data" \
  "$OMAMIX_PRIVATE_ROOT/home" \
  "$OMAMIX_PRIVATE_ROOT/run" \
  "$OMAMIX_SCRATCH/log" \
  "$OMAMIX_SCRATCH/out"

# Exports and the runtime guard come before any config is copied.
export XDG_RUNTIME_DIR="$OMAMIX_PRIVATE_ROOT/runtime"
export XDG_CONFIG_HOME="$OMAMIX_PRIVATE_ROOT/config"
export XDG_STATE_HOME="$OMAMIX_PRIVATE_ROOT/state"
export XDG_DATA_HOME="$OMAMIX_PRIVATE_ROOT/data"
export HOME="$OMAMIX_PRIVATE_ROOT/home"
unset PIPEWIRE_REMOTE PIPEWIRE_RUNTIME_DIR PULSE_RUNTIME_PATH \
  PIPEWIRE_DEBUG WIREPLUMBER_DEBUG || true
export PIPEWIRE_LOG_SYSTEMD=false
if [ "${DISABLE_RTKIT+x}" = x ]; then
  OMAMIX_HAD_DISABLE_RTKIT=1
  OMAMIX_OLD_DISABLE_RTKIT="$DISABLE_RTKIT"
fi
export DISABLE_RTKIT=1

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
cp "$OMAMIX_REPO/docs/private-pipewire.conf" \
  "$XDG_CONFIG_HOME/pipewire/private-pipewire.conf" \
  || die "could not copy the pipewire config"
cp "$OMAMIX_REPO/docs/omamix-private-pulse.conf" \
  "$XDG_CONFIG_HOME/pipewire/omamix-private-pulse.conf" \
  || die "could not copy the pulse config"
set_rtkit_disabled "$XDG_CONFIG_HOME/pipewire/private-pipewire.conf"
set_rtkit_disabled "$XDG_CONFIG_HOME/pipewire/omamix-private-pulse.conf"
echo "private configs set rtkit.enabled = false"

dbus_socket=$XDG_RUNTIME_DIR/omamix-private-bus
case "$dbus_socket" in
  /run/user | /run/user/*) die "refusing a session bus under /run/user" ;;
esac

start_bg() {
  name=$1
  shift
  # Monitor mode only for this spawn. The daemon gets its own process group,
  # so cleanup can signal the group from the pidfile. The script stays in the
  # terminal's foreground group, and a Ctrl-C reaches this script. The pid
  # is not printed.
  set -m
  "$@" >"$OMAMIX_SCRATCH/log/${name}.log" 2>&1 </dev/null &
  pid=$!
  disown "$pid" 2>/dev/null || true
  set +m
  printf '%s\n' "$pid" >"$OMAMIX_PRIVATE_ROOT/run/${name}.pid"
}

daemon_alive() {
  local name=$1
  local pidfile=$OMAMIX_PRIVATE_ROOT/run/${name}.pid
  local pid
  [ -f "$pidfile" ] || return 1
  pid=$(cat "$pidfile" 2>/dev/null || true)
  [ -n "$pid" ] || return 1
  kill -0 "$pid" 2>/dev/null
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
  pipewire -c "$XDG_CONFIG_HOME/pipewire/private-pipewire.conf"

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

if ! command -v wireplumber >/dev/null 2>&1; then
  add_gap "private wireplumber did not start"
else
  start_bg wireplumber \
    env PIPEWIRE_REMOTE="${XDG_RUNTIME_DIR}/omamix-private-0" \
    wireplumber -p omamix-private

  for _ in 1 2 3 4 5 6 7 8 9 10; do
    sleep 0.2
  done
  if daemon_alive wireplumber; then
    OMAMIX_WP_STARTED=1
  else
    add_gap "private wireplumber did not start"
  fi
fi

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
  if [ -f "$OMAMIX_SCRATCH/log/omamix.log" ] \
    && grep -q 'Mix Mic node id' "$OMAMIX_SCRATCH/log/omamix.log" 2>/dev/null; then
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
  wav="$OMAMIX_SCRATCH/out/${kind}.wav"
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

pactl --server "unix:${XDG_RUNTIME_DIR:?}/pulse/omamix-private-pulse" info \
  > "$OMAMIX_SCRATCH/log/pactl-info.txt"

pw-dump -N -r "${XDG_RUNTIME_DIR:?}/omamix-private-0" \
  > "$OMAMIX_SCRATCH/out/private-dump.json"

meta=$(pw-metadata -r "${XDG_RUNTIME_DIR:?}/omamix-private-0" -n default)
# A here-string, not a pipe. Under pipefail, grep -q can close the pipe early
# and the writer dies with SIGPIPE, which looks like a missing key.
if ! grep -F -e 'default.audio.sink' <<< "$meta" >/dev/null; then
  die "default.audio.sink missing"
fi
if ! grep -F -e 'default.audio.source' <<< "$meta" >/dev/null; then
  die "default.audio.source missing"
fi
if grep -F -e 'default.configured' <<< "$meta" >/dev/null; then
  die "default.configured was present before the warning check"
fi

# After the captures: point the private default at Mix Mic. omamix removes
# that node and logs the warning. This stays on the private socket.
pw-metadata -r "${XDG_RUNTIME_DIR:?}/omamix-private-0" \
  -n default 0 default.audio.source '{ "name": "omamix.mix-mic" }' \
  >"$OMAMIX_SCRATCH/log/metadata-set.txt" 2>&1

warned=0
for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
  if [ -f "$OMAMIX_SCRATCH/log/omamix.log" ] \
    && grep -q 'Mix Mic is the default source' "$OMAMIX_SCRATCH/log/omamix.log" 2>/dev/null; then
    warned=1
    break
  fi
  sleep 0.1
done
if [ "$warned" != 1 ]; then
  die "Mix Mic default-source warning was not logged"
fi
echo "Mix Mic default-source warning logged"

python3 - "${XDG_RUNTIME_DIR:?}/omamix-private-0" "$OMAMIX_SCRATCH/log/omamix.log" <<'PY' || die "Mix Mic was still published after the warning"
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

if [ "$OMAMIX_WP_STARTED" = 1 ] && ! daemon_alive wireplumber; then
  add_gap "private wireplumber died"
fi

cleanup || die "private-root processes remained after cleanup"

# The before text is a shell variable, so cleanup can remove every scratch
# file first. Each wpctl status is a new client, so that one line's id and
# pid change every run. Compare the rest. Do not print the status and do not
# write it to a file. A missing or failing wpctl on either side is a gap.
if [ "$OMAMIX_WPCTL_BEFORE" != 1 ]; then
  add_gap "live graph not compared (wpctl before missing or failed)"
else
  wpctl_after_raw=$(env -u PIPEWIRE_REMOTE -u PIPEWIRE_RUNTIME_DIR -u PULSE_RUNTIME_PATH \
    XDG_RUNTIME_DIR="$OMAMIX_REAL_RUNTIME" \
    HOME="$OMAMIX_REAL_HOME" \
    wpctl status) && wpctl_after_ok=1 || wpctl_after_ok=0
  if [ "$wpctl_after_ok" = 1 ]; then
    OMAMIX_WPCTL_AFTER_NORM=$(printf '%s\n' "$wpctl_after_raw" | normalize_wpctl) \
      || die "could not normalize wpctl after"
    unset wpctl_after_raw
    if [ "$OMAMIX_WPCTL_BEFORE_NORM" != "$OMAMIX_WPCTL_AFTER_NORM" ]; then
      unset OMAMIX_WPCTL_BEFORE_NORM OMAMIX_WPCTL_AFTER_NORM
      die "wpctl status changed"
    fi
    echo "wpctl status unchanged"
  else
    unset wpctl_after_raw
    add_gap "live graph not compared (wpctl after missing or failed)"
  fi
fi
unset OMAMIX_WPCTL_BEFORE_NORM OMAMIX_WPCTL_AFTER_NORM

OMAMIX_STAT_AFTER=$(collect_stat) || die "could not stat config trees"
if [ "$OMAMIX_STAT_BEFORE" != "$OMAMIX_STAT_AFTER" ]; then
  echo "stat after:" >&2
  printf '%s\n' "$OMAMIX_STAT_AFTER" >&2
  die "wireplumber or pipewire config mtime changed"
fi
unset OMAMIX_STAT_BEFORE OMAMIX_STAT_AFTER
echo "stat after:"
print_config_mtimes || die "could not stat config trees"

# Directories are already gone. Drop the traps so a late signal cannot change
# the verdict, then say what actually happened.
trap - EXIT INT TERM HUP
if [ "${#OMAMIX_GAPS[@]}" -eq 0 ]; then
  echo "private procedure passed"
  exit 0
fi
joined=
for gap in "${OMAMIX_GAPS[@]}"; do
  if [ -z "$joined" ]; then
    joined=$gap
  else
    joined="$joined; $gap"
  fi
done
echo "PASSED WITH GAPS: $joined"
exit 3
