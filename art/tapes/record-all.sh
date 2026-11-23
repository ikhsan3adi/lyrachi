#!/usr/bin/env bash
# Records the five mode previews into art/modes/ against a throwaway MPD.
# Requires vhs, ttyd, mpd, ffmpeg, and a working headless chromium.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
export LYRACHI_DEMO_DIR="${LYRACHI_DEMO_DIR:-${TMPDIR:-/tmp}/lyrachi-demo}"
demo="$LYRACHI_DEMO_DIR"
export LYRACHI_BIN="${LYRACHI_BIN:-$(cd "$here/../.." && pwd)/target/debug/lyrachi}"

if [ ! -x "$LYRACHI_BIN" ]; then
    echo "missing $LYRACHI_BIN; run: cargo build" >&2
    exit 1
fi

"$here/setup-demo.sh"
trap '"$here/teardown-demo.sh"' EXIT

export PATH="$demo/bin:$PATH"

cd "$here"
rm -rf ../modes/raw
mkdir -p ../modes/raw
for mode in normal block kinetic cylinder canvas3d; do
    echo "recording $mode"
    LYRACHI_HOME="$demo/lyrachi-home-$mode" vhs "$mode.tape"
done

echo "optimizing"
for mode in normal block kinetic cylinder canvas3d; do
    ffmpeg -loglevel error -y -i "../modes/raw/$mode.gif" \
        -vf "fps=10,scale=1000:-1:flags=lanczos,split[a][b];[a]palettegen=max_colors=128:stats_mode=diff[p];[b][p]paletteuse=dither=bayer:bayer_scale=3:diff_mode=rectangle" \
        -loop 0 "../modes/$mode.gif"
    gifsicle -O3 --lossy=90 -o "../modes/$mode.gif" "../modes/$mode.gif"
done

rm -rf ../modes/raw

echo "done:"
ls -lh ../modes/*.gif
