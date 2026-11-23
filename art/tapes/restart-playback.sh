#!/usr/bin/env bash
# Restarts demo playback from the top so every recording starts at 0:00.
set -euo pipefail

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


command("stop")
command("clear")
command('add "Paper Lanterns - Night River.flac"')
command("play")
sock.sendall(b"close\n")
sock.close()
EOF
