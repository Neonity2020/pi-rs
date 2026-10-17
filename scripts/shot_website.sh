#!/usr/bin/env bash
# Regenerate website/screenshot.png: build the Astro site, serve dist on a
# scratch port, and capture the hero fold with headless Chrome (2x, 1680x945).
#
# Usage: scripts/shot_website.sh
#        CHROME=/path/to/chrome scripts/shot_website.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WEBSITE="$ROOT/website"
OUT="$WEBSITE/screenshot.png"

for tool in npm python3; do
  command -v "$tool" >/dev/null || { echo "error: $tool is required" >&2; exit 1; }
done

CHROME="${CHROME:-}"
if [ -z "$CHROME" ]; then
  for candidate in \
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
    "$(command -v google-chrome || true)" \
    "$(command -v chromium || true)"; do
    if [ -n "$candidate" ] && [ -x "$candidate" ]; then
      CHROME="$candidate"
      break
    fi
  done
fi
[ -n "$CHROME" ] || { echo "error: Chrome not found; set CHROME=<path>" >&2; exit 1; }

# Build first: the screenshot must match the sources, not a stale dist.
[ -d "$WEBSITE/node_modules" ] || (cd "$WEBSITE" && npm install)
(cd "$WEBSITE" && npm run build)

TMP="$(mktemp -d)"
LOG="$TMP/server.log"
SHOT="$TMP/shot.png"
PROFILE="$TMP/chrome-profile"
SERVER=""
cleanup() {
  if [ -n "$SERVER" ]; then
    kill "$SERVER" 2>/dev/null || true
    wait "$SERVER" 2>/dev/null || true
  fi
  rm -rf "$TMP"
}
trap cleanup EXIT

# Port 0: the kernel picks a free port; python prints it before accepting.
python3 -u -m http.server 0 --directory "$WEBSITE/dist" >"$LOG" 2>&1 &
SERVER=$!

PORT=""
for _ in {1..50}; do
  PORT="$(sed -n 's/.*port \([0-9][0-9]*\).*/\1/p' "$LOG" | head -n1)"
  [ -n "$PORT" ] && break
  sleep 0.1
done
[ -n "$PORT" ] || { echo "error: preview server did not start" >&2; cat "$LOG" >&2; exit 1; }

# Chrome sometimes exits nonzero after a successful capture, so trust the file.
"$CHROME" --headless=new --disable-gpu --hide-scrollbars --no-first-run \
  --user-data-dir="$PROFILE" --force-device-scale-factor=2 \
  --window-size=1680,945 --virtual-time-budget=15000 \
  --screenshot="$SHOT" "http://127.0.0.1:$PORT/" || true

[ -s "$SHOT" ] || { echo "error: no screenshot produced" >&2; exit 1; }
mv "$SHOT" "$OUT"
echo "wrote ${OUT#"$ROOT"/}"
