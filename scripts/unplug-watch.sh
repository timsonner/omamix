#!/bin/sh

: "${XDG_RUNTIME_DIR:?}"
: "${HOME:?}"
if [ -n "${PIPEWIRE_REMOTE:-}" ]; then
  echo "unset PIPEWIRE_REMOTE before the unplug watch" >&2
  exit 1
fi

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
