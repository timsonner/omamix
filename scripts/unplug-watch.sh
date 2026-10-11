#!/bin/sh
# Read-only log of the default devices while a cable is unplugged and replugged.
# Does not set a default, change a volume, or write WirePlumber/PipeWire config.
# Usage: scripts/unplug-watch.sh [logfile]
# Stop with Ctrl-C. The log is appended, not truncated.

out=${1:-unplug-watch.log}

while true; do
  {
    date -Is
    printf 'default-source: '
    pactl get-default-source
    printf 'default-sink: '
    pactl get-default-sink
    wpctl status
    echo '----'
  } >>"$out"
  sleep 1
done
