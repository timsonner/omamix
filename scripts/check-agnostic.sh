#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SELF="$(basename "$0")"
fail=0
while IFS= read -r -d '' f; do
  bn="$(basename "$f")"
  if [[ "$bn" == "$SELF" ]]; then
    continue
  fi
  case "$f" in
    *alsa_input.usb-ExampleMic*) continue ;;
  esac
  if grep -qE 'X2u|Shure|XPS15|\bXPS\b|-14\.4|/home/[^/[:space:]]+|\btim\b' "$f"; then
    echo "agnostic guard: $f"
    grep -nE 'X2u|Shure|XPS15|\bXPS\b|-14\.4|/home/[^/[:space:]]+|\btim\b' "$f" || true
    fail=1
  fi
done < <(find "$ROOT/crates" "$ROOT/scripts" -type f ! -name "$SELF" -print0)
exit "$fail"
