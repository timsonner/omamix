#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
wav=$(mktemp "${TMPDIR:-/tmp}/omamix-limit.XXXXXX.wav")
log=$(mktemp "${TMPDIR:-/tmp}/omamix-limit.XXXXXX.txt")
cleanup() {
    rm -f "$wav" "$log"
}
trap cleanup EXIT INT TERM
if [ -z "${OMAMIX_MACHINE:-}" ]; then
    OMAMIX_MACHINE=unlabelled
fi
export OMAMIX_MACHINE
OMAMIX_LIMIT_WAV="$wav" cargo test -p omamix-core --lib -- hot_signal_stays_at_or_under_minus_1_dbtp --nocapture
if ! ffmpeg -hide_banner -i "$wav" -af ebur128=peak=true -f null - >"$log" 2>&1; then
    cat "$log" >&2
    exit 1
fi
tp=$(awk '
    /True peak:/ { section = 1; next }
    section && /Peak:/ {
        for (i = 1; i <= NF; i++) {
            if ($i ~ /^-?[0-9]/) {
                print $i
                exit
            }
        }
    }
' "$log")
if [ -z "$tp" ]; then
    echo "ffmpeg did not report a true peak" >&2
    cat "$log" >&2
    exit 1
fi
echo "MEASURED ffmpeg true peak: ${tp} dBFS profile debug machine ${OMAMIX_MACHINE}"
awk -v tp="$tp" 'BEGIN { if ((tp + 0) > -1) exit 1 }' || {
    echo "true peak ${tp} dBFS is above -1" >&2
    exit 1
}
