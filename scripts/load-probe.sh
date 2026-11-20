#!/usr/bin/env bash
set -euo pipefail
if [ -z "${BASH_VERSION:-}" ]; then
  echo "scripts/load-probe.sh needs bash" >&2
  exit 1
fi
if [ "${BASH_SOURCE[0]}" != "$0" ]; then
  echo "execute scripts/load-probe.sh; do not source it" >&2
  return 1
fi

REPO=$(cd "$(dirname "$0")/.." && pwd)
SECONDS_N=${OMAMIX_PROBE_SECONDS:-20}
MATRIX=0
THROTTLE=0
RUN_US=200
STOP_US=700
while [ $# -gt 0 ]; do
  case "$1" in
    --seconds)
      SECONDS_N=${2:?}
      shift 2
      ;;
    --matrix)
      MATRIX=1
      shift
      ;;
    --throttle)
      THROTTLE=1
      shift
      ;;
    --run-us)
      RUN_US=${2:?}
      shift 2
      ;;
    --stop-us)
      STOP_US=${2:?}
      shift 2
      ;;
    *)
      echo "unknown argument $1" >&2
      exit 1
      ;;
  esac
done
case "$SECONDS_N" in
  ''|*[!0-9]*) echo "seconds must be a positive integer" >&2; exit 1 ;;
esac
if [ "$SECONDS_N" -lt 1 ]; then
  echo "seconds must be a positive integer" >&2
  exit 1
fi

for tool in python3 pkg-config pipewire pw-dump pw-metadata pw-top; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "load-probe: missing $tool" >&2
    exit 1
  fi
done
if ! pkg-config --atleast-version=1.2 libpipewire-0.3; then
  echo "load-probe: libpipewire >= 1.2 is required" >&2
  exit 1
fi
if [ "$THROTTLE" = 1 ] && ! command -v taskset >/dev/null 2>&1; then
  echo "load-probe: missing taskset" >&2
  exit 1
fi

LIBDIR=$(pkg-config --variable=libdir libpipewire-0.3 || true)
if [ -n "${LIBDIR:-}" ]; then
  export LD_LIBRARY_PATH="${LIBDIR}${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
fi

BIN=${OMAMIX_BIN:-$REPO/target/release/omamix}
if [ ! -x "$BIN" ]; then
  cargo build --release -p omamix --no-default-features --features pipewire --manifest-path "$REPO/Cargo.toml"
fi
if [ ! -x "$BIN" ]; then
  echo "load-probe: release binary is missing" >&2
  exit 1
fi

SLOTS=$(sed -n 's/^pub const STRIP_SLOT_COUNT: usize = \([0-9][0-9]*\);/\1/p' "$REPO/crates/omamix-core/src/params.rs" | head -n 1)
if [ -z "$SLOTS" ] || [ "$SLOTS" -lt 1 ]; then
  echo "load-probe: could not read the strip slot count" >&2
  exit 1
fi

REAL_HOME=${HOME:-}
REAL_RUNTIME=${XDG_RUNTIME_DIR:-}
ROOT=$(mktemp -d /tmp/omamix-probe.XXXXXX)
SCRATCH=$(mktemp -d /tmp/omamix-probe-scratch.XXXXXX)
case "$ROOT" in
  /tmp/omamix-probe.*) ;;
  *) echo "load-probe: unexpected private root" >&2; exit 1 ;;
esac
case "$SCRATCH" in
  /tmp/omamix-probe-scratch.*) ;;
  *) echo "load-probe: unexpected scratch dir" >&2; exit 1 ;;
esac
if [ -n "$REAL_RUNTIME" ] && [ "$ROOT" = "$REAL_RUNTIME" ]; then
  echo "load-probe: private root must not be the login runtime" >&2
  exit 1
fi

install -d "$ROOT/runtime" "$ROOT/config" "$ROOT/state" "$ROOT/data" "$ROOT/home" "$ROOT/run" "$SCRATCH/log"
export XDG_RUNTIME_DIR="$ROOT/runtime"
export XDG_CONFIG_HOME="$ROOT/config"
export XDG_STATE_HOME="$ROOT/state"
export XDG_DATA_HOME="$ROOT/data"
export HOME="$ROOT/home"
unset PIPEWIRE_REMOTE || true
unset PIPEWIRE_RUNTIME_DIR || true
unset PULSE_RUNTIME_PATH || true
unset PULSE_SERVER || true
unset PIPEWIRE_DEBUG || true
export PIPEWIRE_LOG_SYSTEMD=false
export DISABLE_RTKIT=1
case "$XDG_RUNTIME_DIR" in
  /run/user|/run/user/*) echo "load-probe: refusing a runtime under /run/user" >&2; exit 1 ;;
esac
SOCKET=$XDG_RUNTIME_DIR/omamix-private-0
FAIL=0
CLEANED=0

signal_tree() {
  sig=$1
  if [ -d "$ROOT/run" ]; then
    for pidfile in "$ROOT"/run/*.pid; do
      [ -f "$pidfile" ] || continue
      pid=$(cat "$pidfile" 2>/dev/null || true)
      if [ -n "${pid:-}" ]; then
        if [ "$sig" = TERM ] || [ "$sig" = KILL ]; then
          kill -CONT "$pid" 2>/dev/null || true
        fi
        kill "-$sig" -- "-$pid" 2>/dev/null || kill "-$sig" "$pid" 2>/dev/null || true
      fi
    done
  fi
}

cleanup() {
  [ "$CLEANED" = 1 ] && return 0
  CLEANED=1
  signal_tree TERM
  sleep 0.2
  signal_tree KILL
  wait 2>/dev/null || true
  unset XDG_RUNTIME_DIR XDG_CONFIG_HOME XDG_STATE_HOME XDG_DATA_HOME \
    PIPEWIRE_REMOTE PIPEWIRE_RUNTIME_DIR PULSE_RUNTIME_PATH PULSE_SERVER \
    PIPEWIRE_LOG_SYSTEMD PIPEWIRE_DEBUG DISABLE_RTKIT || true
  if [ -n "$REAL_HOME" ]; then
    export HOME=$REAL_HOME
  fi
  if [ -n "$REAL_RUNTIME" ]; then
    export XDG_RUNTIME_DIR=$REAL_RUNTIME
  fi
  case "$ROOT" in
    /tmp/omamix-probe.*) rm -rf -- "$ROOT" ;;
  esac
  case "$SCRATCH" in
    /tmp/omamix-probe-scratch.*) rm -rf -- "$SCRATCH" ;;
  esac
}
trap 'cleanup; trap - EXIT INT TERM HUP; exit 130' INT
trap 'cleanup; trap - EXIT INT TERM HUP; exit 143' TERM
trap 'cleanup; trap - EXIT INT TERM HUP; exit 129' HUP
trap cleanup EXIT

write_conf() {
  python3 - "$REPO/docs/private-pipewire.conf" "$1" "$2" "$3" "$SLOTS" <<'PY'
import pathlib, sys
src, dst, rate, quantum, slots = sys.argv[1:]
text = pathlib.Path(src).read_text()
old_rate = "default.clock.rate = 48000"
old_quantum = "default.clock.quantum = 1024"
if old_rate not in text or old_quantum not in text:
    sys.exit("private pipewire config has no clock defaults")
text = text.replace(old_rate, "default.clock.rate = " + rate, 1)
text = text.replace(old_quantum, "default.clock.quantum = " + quantum, 1)
needle = "    core.daemon = true\n"
extra = (
    "    default.clock.allowed-rates = [ 44100 48000 96000 ]\n"
    "    default.clock.min-quantum = 32\n"
    "    default.clock.max-quantum = 2048\n"
)
if needle not in text:
    sys.exit("private pipewire config missing daemon flag")
text = text.replace(needle, extra + needle, 1)
old_source = '{ key = default.audio.source value = "{ \\"name\\": \\"private-source\\" }" }'
new_source = '{ key = default.audio.source value = "{ \\"name\\": \\"probe-in-0\\" }" }'
if old_source not in text:
    sys.exit("private pipewire config missing default source")
text = text.replace(old_source, new_source, 1)
profiler = "    { name = libpipewire-module-profiler\n        flags = [ ifexists nofail ] }\n"
link_mod = "    { name = libpipewire-module-link-factory }\n"
if link_mod not in text:
    sys.exit("private pipewire config missing link factory")
text = text.replace(link_mod, link_mod + profiler, 1)
blocks = []
for index in range(int(slots)):
    blocks.append(
        "    { factory = adapter\n"
        "        args = {\n"
        "            factory.name     = support.null-audio-sink\n"
        "            node.name        = probe-in-%d\n"
        "            node.description = \"Probe Input %d\"\n"
        "            media.class      = Audio/Source/Virtual\n"
        "            audio.position   = [ FL FR ]\n"
        "            priority.session = 1000\n"
        "            priority.driver  = 100\n"
        "            object.linger    = true\n"
        "            node.param.PortConfig = \"{ \\\"direction\\\":\\\"Input\\\",\\\"mode\\\":\\\"dsp\\\",\\\"monitor\\\":true,\\\"control\\\":false,\\\"format\\\":{\\\"mediaType\\\":\\\"audio\\\",\\\"mediaSubtype\\\":\\\"raw\\\",\\\"format\\\":\\\"F32P\\\",\\\"channels\\\":2,\\\"position\\\":[\\\"FL\\\",\\\"FR\\\"]}}\"\n"
        "        }\n"
        "    }\n" % (index, index)
    )
blocks.append(
    "    { factory = metadata\n"
    "        args = {\n"
    "            metadata.name = settings\n"
    "        }\n"
    "    }\n"
)
end = text.rfind("]")
if end < 0:
    sys.exit("private pipewire config missing object list")
text = text[:end] + "".join(blocks) + text[end:]
pathlib.Path(dst).write_text(text)
PY
}

start_bg() {
  name=$1
  shift
  set -m
  "$@" >"$SCRATCH/log/${name}.log" 2>&1 </dev/null &
  pid=$!
  set +m
  printf '%s\n' "$pid" >"$ROOT/run/${name}.pid"
  echo "$pid"
}

stop_name() {
  name=$1
  pidfile=$ROOT/run/${name}.pid
  [ -f "$pidfile" ] || return 0
  pid=$(cat "$pidfile" 2>/dev/null || true)
  if [ -n "${pid:-}" ]; then
    kill -CONT "$pid" 2>/dev/null || true
    kill -TERM -- "-$pid" 2>/dev/null || kill -TERM "$pid" 2>/dev/null || true
    sleep 0.2
    kill -KILL -- "-$pid" 2>/dev/null || kill -KILL "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  fi
  rm -f "$pidfile"
}

count_live() {
  python3 - "$SOCKET" <<'PY'
import json, subprocess, sys
socket = sys.argv[1]
raw = subprocess.check_output(["pw-dump", "-N", "-r", socket], text=True)
data = json.loads(raw)
engine = set()
for obj in data:
    if obj.get("type") != "PipeWire:Interface:Node":
        continue
    props = (obj.get("info") or {}).get("props") or {}
    if props.get("node.name") == "omamix.mix-engine":
        engine.add(str(obj.get("id")))
live = 0
for obj in data:
    if obj.get("type") != "PipeWire:Interface:Port":
        continue
    props = (obj.get("info") or {}).get("props") or {}
    if str(props.get("node.id")) not in engine:
        continue
    name = props.get("port.name") or ""
    if name.startswith("in-") and name.endswith("-FL"):
        live += 1
print(live)
PY
}

parse_err() {
  python3 - "$1" <<'PY'
import re, sys
text = open(sys.argv[1], errors="replace").read()
found = []
for line in text.splitlines():
    if "omamix.mix-engine" not in line:
        continue
    match = re.match(
        r"\S+\s+\d+\s+\d+\s+\d+\s+\S+\s+\S+\s+\S+\s+\S+\s+(\d+)\s+",
        line,
    )
    if match:
        found.append(int(match.group(1)))
print(max(found) if found else "na")
PY
}

run_cell() {
  rate=$1
  quantum=$2
  work=$SCRATCH/cell-$rate-$quantum
  mkdir -p "$work" "$SCRATCH/log"
  write_conf "$work/pipewire.conf" "$rate" "$quantum"
  rm -f "$SOCKET"
  stop_name pipewire || true
  stop_name omamix || true
  stop_name slice || true
  stop_name pwtop || true
  start_bg "pipewire-$rate-$quantum" pipewire -c "$work/pipewire.conf" >/dev/null
  pwpid=$(cat "$ROOT/run/pipewire-$rate-$quantum.pid")
  ready=0
  for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32 33 34 35 36 37 38 39 40 41 42 43 44 45 46 47 48 49 50; do
    if [ -S "$SOCKET" ]; then
      ready=1
      break
    fi
    if ! kill -0 "$pwpid" 2>/dev/null; then
      break
    fi
    sleep 0.1
  done
  if [ "$THROTTLE" = 1 ] && [ "$(nproc)" -gt 2 ]; then
    taskset -cp "1-$(($(nproc) - 2))" "$pwpid" >/dev/null 2>&1 || true
  fi
  if [ "$ready" != 1 ]; then
    echo "load-probe: private pipewire socket did not appear at $rate Hz quantum $quantum" >&2
    tail -n 40 "$SCRATCH/log/pipewire-$rate-$quantum.log" >&2 || true
    FAIL=1
    stop_name "pipewire-$rate-$quantum"
    return 0
  fi
  pw-metadata -r "$SOCKET" -n settings 0 clock.force-rate "$rate" >"$work/force-rate.txt" 2>&1 || true
  pw-metadata -r "$SOCKET" -n settings 0 clock.force-quantum "$quantum" >"$work/force-quantum.txt" 2>&1 || true
  sleep 0.3
  args=(--headless --mix-mic --duration "$((SECONDS_N + 90))")
  index=0
  while [ "$index" -lt "$SLOTS" ]; do
    args+=(--strips "device:probe-in-$index")
    index=$((index + 1))
  done
  if [ "$THROTTLE" = 1 ]; then
    set -m
    taskset -c 0 env PIPEWIRE_REMOTE="$SOCKET" "$BIN" "${args[@]}" >"$work/omamix.log" 2>&1 </dev/null &
    ompid=$!
    set +m
  else
    set -m
    env PIPEWIRE_REMOTE="$SOCKET" "$BIN" "${args[@]}" >"$work/omamix.log" 2>&1 </dev/null &
    ompid=$!
    set +m
  fi
  printf '%s\n' "$ompid" >"$ROOT/run/omamix.pid"
  if [ "$THROTTLE" = 1 ]; then
    limit_cpu=0
    if [ "$(nproc)" -gt 1 ]; then
      limit_cpu=$(($(nproc) - 1))
    fi
    set -m
    taskset -c "$limit_cpu" python3 - "$ompid" "$RUN_US" "$STOP_US" >"$work/slice.log" 2>&1 <<'PY' &
import os, signal, sys, time
pid = int(sys.argv[1])
run_ns = int(sys.argv[2]) * 1000
stop_ns = int(sys.argv[3]) * 1000
while True:
    try:
        os.kill(pid, signal.SIGCONT)
    except ProcessLookupError:
        break
    end = time.perf_counter_ns() + run_ns
    while time.perf_counter_ns() < end:
        pass
    try:
        os.kill(pid, signal.SIGSTOP)
    except ProcessLookupError:
        break
    end = time.perf_counter_ns() + stop_ns
    while time.perf_counter_ns() < end:
        pass
PY
    slicepid=$!
    set +m
    printf '%s\n' "$slicepid" >"$ROOT/run/slice.pid"
  fi
  cap=
  seen=0
  for _ in $(seq 1 600); do
    if ! kill -0 "$ompid" 2>/dev/null; then
      break
    fi
    if [ -f "$work/omamix.log" ] && grep -q "live cap .* at ${rate} Hz quantum ${quantum} " "$work/omamix.log"; then
      seen=1
      break
    fi
    sleep 0.1
  done
  if [ "$seen" != 1 ]; then
    echo "load-probe: live cap was not logged at $rate Hz quantum $quantum" >&2
    tail -n 40 "$work/omamix.log" >&2 || true
    FAIL=1
    stop_name slice
    stop_name omamix
    stop_name "pipewire-$rate-$quantum"
    printf '%s %s %s %s %s %s %s %s %s %s\n' "$rate" "$quantum" "-" "-" "-" "-" "-" "-" "-" "-"
    return 0
  fi
  cap=$(python3 - "$work/omamix.log" "$rate" "$quantum" <<'PY'
import re, sys
text = open(sys.argv[1], errors="replace").read()
want_rate, want_quantum = sys.argv[2], sys.argv[3]
cap = ""
for line in text.splitlines():
    match = re.search(
        r"live cap (\d+) at (\d+) Hz quantum (\d+) budget_us (\d+) master_us (\S*) all_live_us (\S*)",
        line,
    )
    if match and match.group(2) == want_rate and match.group(3) == want_quantum:
        cap = match.group(1)
        tries = match
print(cap)
PY
)
  tries_line=$(python3 - "$work/omamix.log" "$rate" "$quantum" <<'PY'
import re, sys
text = open(sys.argv[1], errors="replace").read()
want_rate, want_quantum = sys.argv[2], sys.argv[3]
last = ""
for line in text.splitlines():
    match = re.search(
        r"live cap (\d+) at (\d+) Hz quantum (\d+) budget_us (\d+) master_us (\S*) all_live_us (\S*)",
        line,
    )
    if match and match.group(2) == want_rate and match.group(3) == want_quantum:
        last = "master_us %s all_live_us %s" % (match.group(5), match.group(6))
print(last)
PY
)
  if [ -z "$cap" ]; then
    echo "load-probe: live cap for $rate Hz quantum $quantum was not logged" >&2
    FAIL=1
    stop_name slice
    stop_name omamix
    stop_name "pipewire-$rate-$quantum"
    printf '%s %s %s %s %s %s %s %s %s %s\n' "$rate" "$quantum" "-" "-" "-" "-" "-" "-" "-" "-"
    return 0
  fi
  live=0
  for _ in $(seq 1 100); do
    live=$(count_live || echo 0)
    if [ "$live" = "$cap" ]; then
      break
    fi
    sleep 0.1
  done
  sleep 1
  set -m
  if [ "$THROTTLE" = 1 ] && [ "$(nproc)" -gt 2 ]; then
    taskset -c 1 pw-top -b -n "$SECONDS_N" -r "$SOCKET" >"$work/pw-top.txt" 2>"$work/pw-top.err" &
  else
    pw-top -b -n "$SECONDS_N" -r "$SOCKET" >"$work/pw-top.txt" 2>"$work/pw-top.err" &
  fi
  toppid=$!
  set +m
  printf '%s\n' "$toppid" >"$ROOT/run/pwtop.pid"
  wait "$toppid" 2>/dev/null || true
  rm -f "$ROOT/run/pwtop.pid"
  sleep 2
  stats=$(python3 - "$work/omamix.log" <<'PY'
import re, sys
text = open(sys.argv[1], errors="replace").read()
last = None
for line in text.splitlines():
    match = re.search(
        r"callback us p50 (\d+) p99 (\d+) max (\d+) n (\d+) over (\d+)",
        line,
    )
    if match:
        last = match
if last is None:
    print("- - - -")
else:
    print(last.group(1), last.group(2), last.group(3), last.group(5))
PY
)
  read -r p50 p99 maxus over <<<"$stats"
  err=$(parse_err "$work/pw-top.txt")
  quantum_us=$((quantum * 1000000 / rate))
  printf '%s %s %s %s %s %s %s %s %s %s\n' \
    "$rate" "$quantum" "$live" "$cap" "$p50" "$p99" "$maxus" "$quantum_us" "$over" "$err"
  if [ -n "$tries_line" ]; then
    printf '%s\n' "$tries_line"
  fi
  if [ "$live" != "$cap" ] || [ "$over" != 0 ] || [ "$err" != 0 ]; then
    FAIL=1
  fi
  stop_name slice
  stop_name omamix
  stop_name "pipewire-$rate-$quantum"
  rm -f "$SOCKET"
}

CPU=$(awk -F: '/^model name/ { sub(/^ +/, "", $2); print $2; exit }' /proc/cpuinfo)
if [ -z "$CPU" ]; then
  CPU=unknown
fi
NPROC=$(nproc)
PWVER=$(pkg-config --modversion libpipewire-0.3)
echo "cpu: $CPU"
echo "nproc: $NPROC"
echo "libpipewire: $PWVER"
if [ "$THROTTLE" = 1 ]; then
  if [ "$(nproc)" -gt 2 ]; then
    taskset -cp "1-$(($(nproc) - 2))" "$$" >/dev/null 2>&1 || true
  fi
  echo "throttle: taskset -c 0 and ${RUN_US}us run / ${STOP_US}us stop"
else
  echo "throttle: off"
fi
printf '%s\n' "rate quantum live cap p50_us p99_us max_us quantum_us xrun err"

if [ "$MATRIX" = 1 ]; then
  for rate in 44100 48000 96000; do
    for quantum in 64 256 1024; do
      run_cell "$rate" "$quantum"
    done
  done
else
  run_cell 48000 256
fi

if [ "$FAIL" != 0 ]; then
  for log in "$SCRATCH"/cell-*/omamix.log "$SCRATCH"/log/*.log; do
    [ -f "$log" ] || continue
    echo "load-probe: tail $log" >&2
    tail -n 30 "$log" >&2 || true
  done
  exit 1
fi
