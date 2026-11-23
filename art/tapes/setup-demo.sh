#!/usr/bin/env bash
# Builds a throwaway MPD + Lyrachi demo tree and starts MPD on port 6601.
# Everything lives under LYRACHI_DEMO_DIR (default: ${TMPDIR:-/tmp}/lyrachi-demo),
# so nothing demo-related is written into the repository. One Lyrachi home is
# generated per display mode so every recording starts in the right mode.
set -euo pipefail

demo="${LYRACHI_DEMO_DIR:-${TMPDIR:-/tmp}/lyrachi-demo}"
here="$(cd "$(dirname "$0")" && pwd)"

rm -rf "$demo"
mkdir -p "$demo/music" "$demo/playlists" "$demo/bin"

ffmpeg -loglevel error -f lavfi -i "sine=frequency=220:duration=90" \
    -ac 2 -ar 44100 \
    -metadata artist="Paper Lanterns" -metadata title="Night River" \
    "$demo/music/Paper Lanterns - Night River.flac"
cp "$here/demo.ttml" "$demo/music/Paper Lanterns - Night River.ttml"

ln -s "${LYRACHI_BIN:?LYRACHI_BIN must point at a built lyrachi binary}" \
    "$demo/bin/lyrachi"

cat > "$demo/mpd.conf" <<EOF
music_directory    "$demo/music"
playlist_directory "$demo/playlists"
db_file            "$demo/mpd.db"
state_file         "$demo/mpd.state"
sticker_file       "$demo/mpd.sticker"
log_file           "$demo/mpd.log"
pid_file           "$demo/mpd.pid"
port               "6601"
bind_to_address    "127.0.0.1"

audio_output {
    type "null"
    name "silent output"
}
EOF

style_toml="$(cat "$here/demo-style.toml")"

for mode in normal block kinetic cylinder canvas3d; do
    home="$demo/lyrachi-home-$mode"
    mkdir -p "$home/config/styles"

    mode_config=""
    case "$mode" in
        cylinder) mode_config=$'\n[ui.cylinder]\nspeed_pct = 270\n' ;;
        canvas3d) mode_config=$'\n[ui.canvas3d]\nspeed_pct = 50\nzoom_pct = 50\ntilt_deg = 50\nspawn_y_pct = 95\n' ;;
    esac

    cat > "$home/config/config.toml" <<EOF
[providers]
local_dirs = ["$demo/music"]

[media]
backends = ["mpd"]

[media.mpd]
enabled = true
port = 6601

[ui]
display_mode = "$mode"
style = "demo"$mode_config
EOF

    printf '%s\n' "$style_toml" > "$home/config/styles/demo.toml"
done

mpd "$demo/mpd.conf"

python3 - <<'EOF'
import socket
import sys
import time


def connect():
    for _ in range(50):
        try:
            return socket.create_connection(("127.0.0.1", 6601), timeout=1)
        except OSError:
            time.sleep(0.1)
    sys.exit("mpd did not come up on 6601")


sock = connect()
sock.recv(4096)  # greeting: OK MPD <version>


def command(line):
    sock.sendall((line + "\n").encode())
    data = b""
    while not data.endswith(b"OK\n"):
        chunk = sock.recv(4096)
        if not chunk:
            sys.exit("mpd closed the connection")
        data += chunk
    return data.decode(errors="replace")


command("update")
for _ in range(100):
    status = command("status")
    if "updating_db" not in status:
        break
    time.sleep(0.1)
else:
    sys.exit("mpd database update never finished")

command('add "Paper Lanterns - Night River.flac"')
command("repeat 1")
command("play")
sock.sendall(b"close\n")
sock.close()
print("mpd playing")
EOF
