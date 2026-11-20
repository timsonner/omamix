#!/usr/bin/env bash
set -euo pipefail
if [ -z "${BASH_VERSION:-}" ]; then
  echo "scripts/ci-smoke.sh needs bash" >&2
  exit 1
fi
if [ "${BASH_SOURCE[0]}" != "$0" ]; then
  echo "execute scripts/ci-smoke.sh; do not source it" >&2
  return 1
fi
ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"
for tool in cargo pipewire pw-dump python3 pkg-config; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "ci-smoke: missing $tool" >&2
    exit 1
  fi
done
if ! pkg-config --atleast-version=1.2 libpipewire-0.3; then
  found=$(pkg-config --modversion libpipewire-0.3 2>/dev/null || echo "not found")
  echo "ci-smoke: libpipewire >= 1.2 is required (found $found)" >&2
  exit 1
fi
libdir=$(pkg-config --variable=libdir libpipewire-0.3 || true)
if [ -n "${libdir:-}" ]; then
  export LD_LIBRARY_PATH="${libdir}${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
fi
echo "ci-smoke: building omamix with --features pipewire" >&2
cargo build -p omamix --features pipewire
BIN=$ROOT/target/debug/omamix
if [ ! -x "$BIN" ]; then
  echo "ci-smoke: build did not produce $BIN" >&2
  exit 1
fi
RATE=48000
RUNTIME=$(mktemp -d /tmp/omamix-smoke.XXXXXX)
export XDG_RUNTIME_DIR=$RUNTIME
export XDG_CONFIG_HOME=$RUNTIME/config
export XDG_STATE_HOME=$RUNTIME/state
export XDG_DATA_HOME=$RUNTIME/data
export PIPEWIRE_LOG_SYSTEMD=false
export DISABLE_RTKIT=1
unset PIPEWIRE_REMOTE || true
unset PIPEWIRE_RUNTIME_DIR || true
unset PULSE_RUNTIME_PATH || true
unset PIPEWIRE_DEBUG || true
mkdir -p "$XDG_CONFIG_HOME" "$XDG_STATE_HOME" "$XDG_DATA_HOME"
SOCKET=$RUNTIME/omamix-private-0
pwpid=
ompid=
cleanup() {
  if [ -n "${ompid:-}" ]; then
    kill "$ompid" 2>/dev/null || true
    wait "$ompid" 2>/dev/null || true
  fi
  if [ -n "${pwpid:-}" ]; then
    kill "$pwpid" 2>/dev/null || true
    wait "$pwpid" 2>/dev/null || true
  fi
  rm -rf "$RUNTIME"
}
trap cleanup EXIT
python3 - "$ROOT/docs/private-pipewire.conf" "$RUNTIME/pipewire.conf" "$RATE" <<'PY'
import pathlib, sys
src, dst, rate = sys.argv[1:]
text = pathlib.Path(src).read_text()
clock = "default.clock.rate = " + rate
quantum = "default.clock.quantum = 1024"
if clock not in text or quantum not in text:
    sys.exit("ci-smoke: private pipewire config is not " + rate + " Hz")
if "support.null-audio-sink" not in text:
    sys.exit("ci-smoke: private pipewire config has no null device")
pathlib.Path(dst).write_text(text)
PY
pipewire -c "$RUNTIME/pipewire.conf" >"$RUNTIME/pipewire.log" 2>&1 &
pwpid=$!
ready=0
for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30; do
  if [ -S "$SOCKET" ]; then
    ready=1
    break
  fi
  if ! kill -0 "$pwpid" 2>/dev/null; then
    break
  fi
  sleep 0.1
done
if [ "$ready" != 1 ]; then
  echo "ci-smoke: private pipewire socket did not appear" >&2
  tail -n 40 "$RUNTIME/pipewire.log" >&2 || true
  exit 1
fi
PIPEWIRE_REMOTE=$SOCKET "$BIN" --headless --mix-mic --duration 8 >"$RUNTIME/omamix.log" 2>&1 &
ompid=$!
set +e
python3 - "$SOCKET" "$RUNTIME" "$ompid" "$RATE" <<'PY'
import json, os, subprocess, sys, time

socket, work, ompid_s, rate_s = sys.argv[1:]
rate = int(rate_s)
ompid = int(ompid_s)
chain = int(round(rate * 0.005))
tol = max(1, rate // 1000)

def dump():
    raw = subprocess.check_output(["pw-dump", "-N", "-r", socket], text=True)
    return json.loads(raw)

def nodes(data):
    return [o for o in data if o.get("type") == "PipeWire:Interface:Node"]

def ports(data):
    return [o for o in data if o.get("type") == "PipeWire:Interface:Port"]

def props(obj):
    return (obj.get("info") or {}).get("props") or {}

def params(obj):
    return (obj.get("info") or {}).get("params") or {}

def find_name(data, name):
    for obj in nodes(data):
        if props(obj).get("node.name") == name:
            return obj
    return None

def port_id(data, node_id, name):
    for obj in ports(data):
        p = props(obj)
        if str(p.get("node.id")) != str(node_id):
            continue
        if p.get("port.name") == name or p.get("audio.channel") == name:
            return obj["id"]
    return None

def param_items(obj, key):
    blob = params(obj)
    if isinstance(blob, dict):
        return blob.get(key) or []
    if isinstance(blob, list):
        return [item for item in blob if isinstance(item, dict) and item.get("id") == key]
    return []

def latency_values(item):
    if not isinstance(item, dict):
        return []
    found = []
    for key in ("minRate", "maxRate"):
        value = item.get(key)
        if isinstance(value, int):
            found.append(value)
    if not found and isinstance(item.get("rate"), int):
        found.append(item["rate"])
    return found

def alive(pid):
    try:
        os.kill(pid, 0)
    except OSError:
        return False
    return True

def fail(message):
    print(f"ci-smoke: {message}", file=sys.stderr)
    sys.exit(1)

def capture_rows(data, node_id):
    rows = []
    for obj in ports(data):
        p = props(obj)
        if str(p.get("node.id")) != str(node_id):
            continue
        if p.get("port.direction") != "out":
            continue
        name = p.get("port.name") or ""
        if not str(name).startswith("capture_"):
            continue
        values = []
        for item in param_items(obj, "Latency"):
            if not isinstance(item, dict):
                continue
            direction = item.get("direction")
            if direction not in (None, "Output"):
                continue
            values.extend(latency_values(item))
        rows.append((name, values))
    return rows

def process_latency(data, name):
    node = find_name(data, name)
    if node is None:
        return None
    for item in param_items(node, "ProcessLatency"):
        if isinstance(item, dict) and item.get("rate") not in (None,):
            return int(item["rate"])
    return None

def scalar_rate(value):
    if isinstance(value, bool) or value in (None, 0):
        return None
    if isinstance(value, int):
        return value
    if isinstance(value, float) and value.is_integer():
        return int(value)
    if isinstance(value, str) and value.isdigit():
        return int(value)
    if isinstance(value, dict):
        default = value.get("default")
        low = value.get("min")
        high = value.get("max")
        if isinstance(default, int) and default > 0 and low == high:
            return default
        num = value.get("num")
        denom = value.get("denom")
        if isinstance(num, int) and isinstance(denom, int) and denom != 0:
            if num % denom == 0:
                return num // denom
    return None

def format_rate(node):
    if node is None:
        return None
    blob = params(node)
    if isinstance(blob, dict):
        groups = [blob.get("Format") or [], blob.get("EnumFormat") or []]
    elif isinstance(blob, list):
        groups = [blob]
    else:
        return None
    for group in groups:
        for item in group:
            if isinstance(item, dict):
                found = scalar_rate(item.get("rate"))
                if found:
                    return found
    return None

def clock_rate(data):
    for obj in data:
        info = obj.get("info") or {}
        for bag in (info.get("props") or {}, info):
            if not isinstance(bag, dict):
                continue
            for key in ("clock.rate", "default.clock.rate"):
                found = scalar_rate(bag.get(key))
                if found:
                    return found
    return None

mix = None
fl = None
fr = None
deadline = time.time() + 5
while time.time() < deadline and alive(ompid):
    data = dump()
    mix = find_name(data, "omamix.mix-mic")
    if mix is not None:
        fl = port_id(data, mix["id"], "capture_FL")
        fr = port_id(data, mix["id"], "capture_FR")
        if fl is not None and fr is not None:
            break
    time.sleep(0.05)

if not alive(ompid):
    log = os.path.join(work, "omamix.log")
    text = ""
    if os.path.isfile(log):
        text = open(log, "r", errors="replace").read()[-2000:]
    fail("omamix exited before Mix Mic could be checked\n" + text)

data = dump()
mix = find_name(data, "omamix.mix-mic")
if mix is None:
    fail("Mix Mic node did not appear")
fl = port_id(data, mix["id"], "capture_FL")
fr = port_id(data, mix["id"], "capture_FR")
if fl is None or fr is None:
    names = []
    for obj in ports(data):
        p = props(obj)
        if str(p.get("node.id")) == str(mix["id"]):
            names.append(str(p.get("port.name")))
    shown = ", ".join(names) if names else "none"
    fail(f"Mix Mic capture ports did not appear (ports: {shown})")

graph = clock_rate(data)
node_rate = format_rate(mix)
if graph is not None and graph != rate:
    fail(f"private graph rate is {graph}, expected {rate}")
if node_rate is not None and node_rate != rate:
    fail(f"Mix Mic format rate is {node_rate}, expected {rate}")

def close_enough(value):
    return abs(int(value) - chain) <= tol

def latency_ready(rows, engine):
    if not rows or engine is None or not close_enough(engine):
        return False
    for _name, values in rows:
        if not values or any(not close_enough(value) for value in values):
            return False
    return True

rows = []
engine = None
deadline = time.time() + 4
while time.time() < deadline and alive(ompid):
    data = dump()
    mix = find_name(data, "omamix.mix-mic")
    if mix is None:
        time.sleep(0.05)
        continue
    rows = capture_rows(data, mix["id"])
    engine = process_latency(data, "omamix.mix-engine")
    if latency_ready(rows, engine):
        break
    time.sleep(0.1)

if not rows:
    fail("Mix Mic capture ports were not listed")
missing = [name for name, values in rows if not values]
if missing:
    fail("Mix Mic capture ports did not report latency: " + ", ".join(missing))

bad = []
for name, values in rows:
    for value in values:
        if not close_enough(value):
            bad.append(f"{name}={value}")
engine_bad = engine is None or not close_enough(engine)
if bad or engine_bad:
    shown = ", ".join(f"{name}={values}" for name, values in rows)
    fail(
        f"reported port latency {shown} does not match chain latency {chain} samples "
        f"at {rate} Hz (engine ProcessLatency {engine}, tolerance {tol} samples)"
    )

shown = ", ".join(f"{name}={values[0]}" for name, values in rows)
print(
    f"ci-smoke: Mix Mic node present, capture ports present, "
    f"port latency {shown} matches chain latency {chain} samples at {rate} Hz "
    f"(engine ProcessLatency {engine}, tolerance {tol} samples)"
)
PY
status=$?
set -e
if [ "$status" != 0 ]; then
  echo "ci-smoke: omamix log:" >&2
  tail -n 40 "$RUNTIME/omamix.log" >&2 || true
  echo "ci-smoke: pipewire log:" >&2
  tail -n 40 "$RUNTIME/pipewire.log" >&2 || true
  exit "$status"
fi
exit 0
