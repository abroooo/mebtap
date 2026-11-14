#!/usr/bin/env bash
# One-command demo: synthesize a drive, uplink it, and render its poster.
#
# `ecu-sim --replay-stage` back-dates each run to end "now", so the drive's time
# window drifts every time and poster.py needs that exact window. This wrapper
# runs the replay, captures the printed UTC window, and hands it straight to
# poster.py — so a fresh clone goes from nothing to a poster in one command:
#
#   scripts/demo-poster.sh                 # embedded synthetic route
#   scripts/demo-poster.sh path/to.gpx     # any GPX track
#
# Each run uses a UNIQUE device_id by default so repeated runs can never land in
# the same poster window (every drive is back-dated to end "now", so windows of
# successive runs overlap). Pin DEVICE=... to reuse one id on purpose.
#
# Prereqs: the host stack must be up (docker-compose up -d + the backend on
# :3000). Env overrides: UPLINK, DEVICE, DEVICE_KEY, OUT, DATABASE_URL.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"

UPLINK="${UPLINK:-http://localhost:3000}"
DEVICE="${DEVICE:-demo-$(date +%Y%m%d-%H%M%S)}"
KEY="${DEVICE_KEY:-dev}"
OUT="${OUT:-demo-poster.png}"

gpx_arg=()
[ "$#" -ge 1 ] && gpx_arg=(--gpx "$1")

echo ">> synthesizing + uplinking demo drive (device=$DEVICE) ..."
out="$(cargo run -q -p ecu-sim -- --replay-stage \
        --uplink "$UPLINK" --device-key "$KEY" --device-id "$DEVICE" \
        "${gpx_arg[@]}")"
echo "$out"

win="$(printf '%s\n' "$out" | grep 'drive window (UTC epoch s)' || true)"
[ -n "$win" ] || { echo "!! replay did not report a drive window (uplink failed?)" >&2; exit 1; }

a="$(printf '%s' "$win" | sed -E 's/.*: ([0-9]+) \.\. ([0-9]+).*/\1/')"
b="$(printf '%s' "$win" | sed -E 's/.*: ([0-9]+) \.\. ([0-9]+).*/\2/')"
# Pad ±60 s so edge samples are never clipped; render in UTC so the naive
# window strings line up with the UTC timestamps stored in the DB (PGTZ=UTC).
start="$(date -u -d "@$((a - 60))" '+%Y-%m-%d %H:%M:%S')"
end="$(date -u -d "@$((b + 60))" '+%Y-%m-%d %H:%M:%S')"

echo ">> rendering poster for UTC window $start .. $end ..."
PGTZ=UTC scripts/poster.py "$start" "$end" --device "$DEVICE" --out "$OUT"
echo ">> wrote $OUT"
