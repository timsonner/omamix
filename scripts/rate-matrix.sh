#!/usr/bin/env bash
set -euo pipefail
if [ -z "${BASH_VERSION:-}" ]; then
  echo "scripts/rate-matrix.sh needs bash" >&2
  exit 1
fi
if [ "${BASH_SOURCE[0]}" != "$0" ]; then
  echo "execute scripts/rate-matrix.sh; do not source it" >&2
  return 1
fi
ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${OMAMIX_BIN:-$ROOT/target/debug/omamix}
if [ ! -x "$BIN" ]; then
  echo "UNVERIFIED missing binary $BIN" >&2
  exit 1
fi
for tool in pipewire pw-dump pw-record pw-link pw-cli pw-metadata python3; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "UNVERIFIED missing $tool" >&2
    exit 1
  fi
done
if command -v pkg-config >/dev/null 2>&1 && pkg-config --exists libpipewire-0.3; then
  libdir=$(pkg-config --variable=libdir libpipewire-0.3 || true)
  if [ -n "${libdir:-}" ]; then
    export LD_LIBRARY_PATH="${libdir}${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
  fi
fi
RUNTIME=$(mktemp -d /tmp/omamix-rate.XXXXXX)
SCRATCH=$(mktemp -d /tmp/omamix-scratch.XXXXXX)
export XDG_RUNTIME_DIR=$RUNTIME
export PIPEWIRE_LOG_SYSTEMD=false
export DISABLE_RTKIT=1
unset PIPEWIRE_REMOTE || true
unset PIPEWIRE_RUNTIME_DIR || true
unset PULSE_RUNTIME_PATH || true
unset PIPEWIRE_DEBUG || true
SOCKET=$RUNTIME/omamix-private-0
fail=0
cleanup() {
  rm -rf "$RUNTIME" "$SCRATCH"
}
trap cleanup EXIT

run_cell() {
  local rate=$1 quantum=$2
  local work=$SCRATCH/cell-$rate-$quantum
  mkdir -p "$work"
  python3 - "$ROOT/docs/private-pipewire.conf" "$work/pipewire.conf" "$rate" "$quantum" <<'PY'
import pathlib, sys
src, dst, rate, quantum = sys.argv[1:]
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
settings = (
    "    { factory = metadata\n"
    "        args = {\n"
    "            metadata.name = settings\n"
    "        }\n"
    "    }\n"
)
end = text.rfind("]")
if end < 0:
    sys.exit("private pipewire config missing object list")
text = text[:end] + settings + text[end:]
pathlib.Path(dst).write_text(text)
PY
  if [ ! -s "$work/pipewire.conf" ]; then
    echo "UNVERIFIED rate=$rate quantum=$quantum pipewire config was not written"
    return 1
  fi
  rm -f "$SOCKET"
  pipewire -c "$work/pipewire.conf" >"$work/pipewire.log" 2>&1 &
  local pwpid=$!
  local ompid=
  local recpid=
  local ready=0
  local i
  for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30; do
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
    echo "UNVERIFIED rate=$rate quantum=$quantum private pipewire socket did not appear"
    tail -n 40 "$work/pipewire.log" >&2 || true
    kill "$pwpid" 2>/dev/null || true
    wait "$pwpid" 2>/dev/null || true
    return 1
  fi
  pw-metadata -r "$SOCKET" -n settings 0 clock.force-rate "$rate" >"$work/force-rate.txt" 2>&1 || true
  pw-metadata -r "$SOCKET" -n settings 0 clock.force-quantum "$quantum" >"$work/force-quantum.txt" 2>&1 || true
  sleep 0.2
  local stdin_fifo=$work/stdin
  mkfifo "$stdin_fifo"
  exec {omamix_stdin}<>"$stdin_fifo"
  PIPEWIRE_REMOTE=$SOCKET "$BIN" --headless --mix-mic --tone --duration 8 <"$stdin_fifo" >"$work/omamix.log" 2>&1 &
  ompid=$!
  python3 - "$SOCKET" "$work" "$rate" "$quantum" "$ompid" "$stdin_fifo" <<'PY'
import json, math, os, re, signal, struct, subprocess, sys, time, wave

socket, work, rate_s, quantum_s, ompid_s, fifo = sys.argv[1:]
rate = int(rate_s)
quantum = int(quantum_s)
ompid = int(ompid_s)
wav = os.path.join(work, "tone.wav")

def sh(*args, check=True):
    return subprocess.run(args, text=True, capture_output=True, check=check)

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

def find_recorder(data):
    for obj in nodes(data):
        p = props(obj)
        binary = p.get("application.process.binary") or ""
        name = p.get("application.name") or ""
        base = os.path.basename(binary)
        if base == "pw-record" or name == "pw-record" or binary.endswith("/pw-record"):
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

def alive(pid):
    try:
        os.kill(pid, 0)
    except OSError:
        return False
    return True

def first_rate(node):
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
            if isinstance(item, dict) and item.get("rate") not in (None, 0):
                return item.get("rate")
    return None

def param_items(obj, key):
    blob = params(obj)
    if isinstance(blob, dict):
        return blob.get(key) or []
    if isinstance(blob, list):
        return [item for item in blob if isinstance(item, dict) and item.get("id") == key]
    return []

def process_latency(data):
    for obj in nodes(data):
        name = props(obj).get("node.name") or ""
        for item in param_items(obj, "ProcessLatency"):
            if isinstance(item, dict) and "rate" in item:
                return name, item.get("rate")
    return None, None

def process_latency_of(data, name):
    node = find_name(data, name)
    if node is None:
        return None
    for item in param_items(node, "ProcessLatency"):
        if isinstance(item, dict) and "rate" in item:
            return item.get("rate")
    return None

def latency_rate(item):
    if not isinstance(item, dict):
        return None
    for key in ("maxRate", "minRate", "rate"):
        if item.get(key) not in (None,):
            return item.get(key)
    return None

def port_latency_rows(data, node_id):
    rows = []
    if node_id is None:
        return rows
    for obj in ports(data):
        p = props(obj)
        if str(p.get("node.id")) != str(node_id):
            continue
        port_direction = p.get("port.direction")
        for item in param_items(obj, "Latency"):
            rate = latency_rate(item)
            if rate is None or not isinstance(item, dict):
                continue
            rows.append((port_direction, item.get("direction"), rate))
    return rows

def same_direction_rates(rows):
    found = []
    for port_direction, lat_direction, rate in rows:
        if port_direction == "out" and lat_direction == "Output":
            found.append(rate)
        elif port_direction == "in" and lat_direction == "Input":
            found.append(rate)
    return found

def capture_output_rates(rows):
    return [
        rate
        for port_direction, lat_direction, rate in rows
        if port_direction == "out" and lat_direction == "Output"
    ]

def latency_snapshot():
    data = dump()
    mix = find_name(data, "omamix.mix-mic")
    engine_node = find_name(data, "omamix.mix-engine")
    engine_lat = process_latency_of(data, "omamix.mix-engine")
    mic_lat = process_latency_of(data, "omamix.mix-mic")
    lat_name, lat_rate = process_latency(data)
    engine_rows = port_latency_rows(data, None if engine_node is None else engine_node.get("id"))
    mic_rows = port_latency_rows(data, None if mix is None else mix.get("id"))
    return data, mix, lat_name, lat_rate, engine_lat, mic_lat, engine_rows, mic_rows

def recorder_link_rates():
    result = sh("pw-link", "-r", socket, "-l", "-t", check=False)
    text = (result.stdout or "") + "\n" + (result.stderr or "")
    open(os.path.join(work, "pw-link.txt"), "w").write(text)
    if result.returncode != 0 and not (result.stdout or "").strip():
        return None
    node_name = ""
    if recorder is not None:
        node_name = props(recorder).get("node.name") or ""
    rates = []
    watch = False
    for line in text.splitlines():
        stripped = line.strip()
        named = bool(node_name) and node_name in stripped and "input_" in stripped
        generic = "pw-record" in stripped and "input_" in stripped
        if named or generic:
            watch = True
            continue
        if watch and "output latency:" in stripped:
            match = re.search(r"rate=\[\s*(-?\d+)\s+(-?\d+)\s*\]", stripped)
            if match:
                rates.append(int(match.group(2)))
            watch = False
            continue
        if watch and stripped and "latency:" not in stripped:
            watch = False
    return rates

rec = subprocess.Popen(
    [
        "pw-record",
        "--target",
        "0",
        "-R",
        socket,
        "--rate",
        str(rate),
        "--channels",
        "2",
        "--format",
        "s16",
        wav,
    ],
    stdout=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL,
)
open(os.path.join(work, "pw-record.pid"), "w").write(str(rec.pid))

mix = None
recorder = None
deadline = time.time() + 2.5
while time.time() < deadline and alive(ompid):
    data = dump()
    mix = find_name(data, "omamix.mix-mic")
    recorder = find_recorder(data)
    if mix is not None and recorder is not None:
        break
    time.sleep(0.05)

if mix is None or recorder is None:
    rec.send_signal(signal.SIGTERM)
    rec.wait(timeout=2)
    print(f"UNVERIFIED rate={rate} quantum={quantum} mix-mic or recorder node missing")
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
        "rate": rate,
        "channels": 2,
        "position": ["FL", "FR"],
    },
})
sh("pw-cli", "-r", socket, "set-param", str(recorder["id"]), "PortConfig", port_config, check=False)
in_fl = in_fr = None
for _ in range(40):
    data = dump()
    in_fl = port_id(data, recorder["id"], "input_FL") or port_id(data, recorder["id"], "FL")
    in_fr = port_id(data, recorder["id"], "input_FR") or port_id(data, recorder["id"], "FR")
    if in_fl is None:
        for obj in ports(data):
            p = props(obj)
            if str(p.get("node.id")) == str(recorder["id"]) and p.get("port.direction") == "in":
                in_fl = obj["id"]
                break
    if in_fl is not None:
        break
    time.sleep(0.05)
if in_fl is None:
    rec.send_signal(signal.SIGTERM)
    rec.wait(timeout=2)
    print(f"UNVERIFIED rate={rate} quantum={quantum} recorder input port missing")
    sys.exit(1)

data = dump()
mix = find_name(data, "omamix.mix-mic")
if mix is None:
    rec.send_signal(signal.SIGTERM)
    rec.wait(timeout=2)
    print(f"UNVERIFIED rate={rate} quantum={quantum} mix-mic disappeared before link")
    sys.exit(1)
out_fl = port_id(data, mix["id"], "capture_FL")
out_fr = port_id(data, mix["id"], "capture_FR")
if out_fl is None or out_fr is None:
    rec.send_signal(signal.SIGTERM)
    rec.wait(timeout=2)
    print(f"UNVERIFIED rate={rate} quantum={quantum} mix-mic capture ports missing")
    sys.exit(1)

def link(src, dst):
    result = sh("pw-link", "-r", socket, str(src), str(dst), check=False)
    if result.returncode != 0:
        text = (result.stderr or "") + (result.stdout or "")
        if "exists" not in text.lower():
            raise SystemExit(f"UNVERIFIED rate={rate} quantum={quantum} link failed")

link(out_fl, in_fl)
if in_fr is not None:
    link(out_fr, in_fr)

time.sleep(0.6)
data, mix, lat_name, lat_rate, engine_lat, mic_lat, engine_rows, mic_rows = latency_snapshot()
node_rate = first_rate(mix)
engine_ports = [rate for _port_direction, _lat_direction, rate in engine_rows]
mic_ports = [rate for _port_direction, _lat_direction, rate in mic_rows]
engine_same = same_direction_rates(engine_rows)
mic_capture = capture_output_rates(mic_rows)
link_values = recorder_link_rates()
meta = sh("pw-metadata", "-r", socket, "-n", "settings", check=False).stdout
open(os.path.join(work, "during.json"), "w").write(json.dumps(data))
with open(fifo, "w") as handle:
    handle.write("bypass limiter\n")
    handle.flush()
bypass_capture = None
bypass_engine = None
bypass_same = None
bypass_deadline = time.time() + 2.0
while time.time() < bypass_deadline and alive(ompid):
    time.sleep(0.1)
    _data_b, _mix_b, _name_b, _rate_b, engine_lat_b, _mic_lat_b, engine_rows_b, mic_rows_b = latency_snapshot()
    rates = capture_output_rates(mic_rows_b)
    same = same_direction_rates(engine_rows_b)
    bypass_capture = rates
    bypass_engine = engine_lat_b
    bypass_same = same
    if (
        rates
        and all(int(value) == 0 for value in rates)
        and engine_lat_b is not None
        and int(engine_lat_b) == 0
        and same
        and all(int(value) == 0 for value in same)
    ):
        open(os.path.join(work, "bypass.json"), "w").write(json.dumps(_data_b))
        break
while alive(ompid):
    time.sleep(0.05)
rec.send_signal(signal.SIGTERM)
try:
    rec.wait(timeout=2)
except subprocess.TimeoutExpired:
    rec.kill()
    rec.wait(timeout=2)
time.sleep(0.3)
after = dump()
open(os.path.join(work, "after.json"), "w").write(json.dumps(after))
left = find_name(after, "omamix.mix-mic")
engine = find_name(after, "omamix.mix-engine")
teardown = "absent" if left is None and engine is None else "present"

tone = "none"
frames = 0
file_rate = 0
onset = None
if os.path.isfile(wav) and os.path.getsize(wav) > 44:
    w = wave.open(wav, "rb")
    frames = w.getnframes()
    file_rate = w.getframerate()
    channels = w.getnchannels()
    width = w.getsampwidth()
    raw = w.readframes(frames)
    w.close()
    if width == 2:
        values = struct.unpack("<" + "h" * (len(raw) // 2), raw)
        scale = 32768.0
    elif width == 4:
        values = struct.unpack("<" + "i" * (len(raw) // 4), raw)
        scale = 2147483648.0
    else:
        values = ()
        scale = 1.0
    mono = [values[i] / scale for i in range(0, len(values), channels or 1)]
    loud = [i for i, sample in enumerate(mono) if abs(sample) >= 0.02]
    onset = loud[0] if loud else None
    if len(loud) >= max(8, file_rate // 10):
        mono = mono[loud[0]:loud[-1] + 1]
        crosses = 0
        sounding = 0
        for i in range(1, len(mono)):
            if mono[i] == 0.0 or mono[i - 1] == 0.0:
                continue
            sounding += 1
            if mono[i - 1] < 0.0 <= mono[i]:
                crosses += 1
        if file_rate and sounding:
            tone = f"{crosses * file_rate / sounding:.2f}"

def meta_has(key, value):
    return f"{key}" in meta and str(value) in meta

force_ok = meta_has("clock.force-rate", rate) and meta_has("clock.force-quantum", quantum)
expect = int(rate * 0.005 + 0.5)
def exact(value):
    return value is not None and int(value) == expect
lat_ok = exact(engine_lat)
port_ok = bool(engine_same) and all(exact(value) for value in engine_same)
mic_ok = bool(mic_capture) and all(exact(value) for value in mic_capture)
bypass_ok = (
    bool(bypass_capture)
    and all(int(value) == 0 for value in bypass_capture)
    and bypass_engine is not None
    and int(bypass_engine) == 0
    and bool(bypass_same)
    and all(int(value) == 0 for value in bypass_same)
)
if not link_values:
    link_tag = "UNVERIFIED"
    link_ok = True
elif all(exact(value) for value in link_values):
    link_tag = "MEASURED"
    link_ok = True
else:
    link_tag = "MEASURED"
    link_ok = False
try:
    tone_ok = abs(float(tone) - 440.0) <= 5.0
except ValueError:
    tone_ok = False
rate_ok = node_rate is not None and int(node_rate) == rate
tear_ok = teardown == "absent"
passed = tone_ok and rate_ok and lat_ok and port_ok and mic_ok and bypass_ok and link_ok and tear_ok and force_ok
tag = "MEASURED" if tone != "none" or node_rate is not None else "UNVERIFIED"
print(
    f"{tag} rate={rate} quantum={quantum} tone_hz={tone} node_rate={node_rate} "
    f"process_latency={lat_rate} process_latency_node={lat_name} "
    f"engine_process_latency={engine_lat} mic_process_latency={mic_lat} "
    f"engine_port_latency={engine_ports} engine_port_same={engine_same} mic_port_latency={mic_ports} "
    f"mic_capture={mic_capture} mic_capture_bypass={bypass_capture} engine_bypass={bypass_engine} "
    f"expect_samples={expect} onset_samples={onset} applied_delay=UNVERIFIED "
    f"link_latency={link_values} link={link_tag} pactl=UNVERIFIED "
    f"file_rate={file_rate} frames={frames} teardown={teardown} "
    f"force_seen={int(force_ok)} port_ok={int(port_ok)} mic_ok={int(mic_ok)} "
    f"bypass_ok={int(bypass_ok)} pass={int(passed)}"
)
sys.exit(0 if passed else 1)
PY
  local status=$?
  kill "$ompid" 2>/dev/null || true
  wait "$ompid" 2>/dev/null || true
  if [ -f "$work/pw-record.pid" ]; then
    recpid=$(cat "$work/pw-record.pid" 2>/dev/null || true)
    if [ -n "$recpid" ]; then
      kill "$recpid" 2>/dev/null || true
      wait "$recpid" 2>/dev/null || true
    fi
  fi
  kill "$pwpid" 2>/dev/null || true
  wait "$pwpid" 2>/dev/null || true
  rm -f "$SOCKET"
  if [ -n "${omamix_stdin:-}" ]; then
    exec {omamix_stdin}>&-
    unset omamix_stdin
  fi
  return "$status"
}

for rate in 44100 48000 96000; do
  for quantum in 32 64 256 1024; do
    if run_cell "$rate" "$quantum"; then
      :
    else
      fail=1
    fi
  done
done
exit "$fail"
