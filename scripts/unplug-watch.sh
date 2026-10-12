#!/bin/sh
# Live-session read only. Does not set a default, a volume, or a config file.
# Pins the runtime directory and home it is given. Refuses a set PIPEWIRE_REMOTE
# so a private socket cannot be selected by accident.
# Usage: scripts/unplug-watch.sh [logfile]
# Stop with Ctrl-C. The log is appended, not truncated.
# The steps and the pass condition are in docs/TESTING.md under "Unplug test".

: "${XDG_RUNTIME_DIR:?}"
: "${HOME:?}"
if [ -n "${PIPEWIRE_REMOTE:-}" ]; then
  echo "unset PIPEWIRE_REMOTE before the unplug watch" >&2
  exit 1
fi

# Default outside the repo. wpctl status lines can carry a device serial.
# The default path is a fresh mktemp name, printed once. An argument still
# selects the log and is not printed.
if [ -n "${1:-}" ]; then
  out=$1
else
  out=$(mktemp /tmp/omamix-unplug.XXXXXX) || exit 1
  printf '%s\n' "$out"
fi
case "$out" in
  /tmp/omamix-fixround | /tmp/omamix-fixround/*)
    echo "refusing /tmp/omamix-fixround" >&2
    exit 1
    ;;
esac

while true; do
  {
    date -Is
    printf 'default-source: '
    XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:?}" HOME="${HOME:?}" pactl get-default-source
    printf 'default-sink: '
    XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:?}" HOME="${HOME:?}" pactl get-default-sink
    XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:?}" HOME="${HOME:?}" wpctl status
    echo '----'
  } >>"$out"
  sleep 1
done
