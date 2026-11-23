#!/usr/bin/env bash
# Stops the demo MPD. The demo tree is left in place for inspection.
set -euo pipefail

demo="${LYRACHI_DEMO_DIR:-${TMPDIR:-/tmp}/lyrachi-demo}"
if [ -f "$demo/mpd.pid" ]; then
    kill "$(cat "$demo/mpd.pid")" 2>/dev/null || true
fi
