#!/usr/bin/env bash
# Build one GitHub release asset for a lyrachi tag.
#
# Usage:
#   packaging/build-release-asset.sh [--target <triple>] [--format tar.gz|zip]
#                                     [--out <dir>] [tag]
#
# The target defaults to the host triple, the format to tar.gz (zip on Windows
# targets), the output directory to the current directory, and the tag to the
# latest one reachable from HEAD. The archive contains the release binary,
# README.md, and LICENSE, which is the layout packaging/aur/lyrachi-bin
# expects. Assets ship the default and experimental providers.
set -euo pipefail

usage() {
  cat <<'EOF'
usage: packaging/build-release-asset.sh [options] [tag]

  --target <triple>    Rust target triple (default: host)
  --format tar.gz|zip  Archive format (default: zip for Windows, else tar.gz)
  --out <dir>          Output directory (default: current directory)
  -h, --help           Show this help
EOF
}

TARGET=""
FORMAT=""
OUT="."
TAG=""

while [ $# -gt 0 ]; do
  case "$1" in
    --target) TARGET="${2:?--target needs a value}"; shift 2 ;;
    --format) FORMAT="${2:?--format needs a value}"; shift 2 ;;
    --out) OUT="${2:?--out needs a value}"; shift 2 ;;
    -h | --help) usage; exit 0 ;;
    -*) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
    *) TAG="$1"; shift ;;
  esac
done

cd "$(git rev-parse --show-toplevel)"

if [ -z "$TARGET" ]; then
  TARGET="$(rustc -vV | sed -n 's/^host: //p')"
fi
if [ -z "$FORMAT" ]; then
  case "$TARGET" in
    *-windows-*) FORMAT="zip" ;;
    *) FORMAT="tar.gz" ;;
  esac
fi
if [ -z "$TAG" ]; then
  TAG="$(git describe --tags --abbrev=0)"
fi

VERSION="${TAG#v}"
BIN="lyrachi"
case "$TARGET" in *-windows-*) BIN="lyrachi.exe" ;; esac
NAME="lyrachi-v${VERSION}-${TARGET}"

mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

export CARGO_INCREMENTAL=0
cargo build --release --locked --features experimental-providers --target "$TARGET"

cp "target/${TARGET}/release/${BIN}" "$STAGE/${BIN}"
chmod 0755 "$STAGE/${BIN}"
cp README.md LICENSE "$STAGE/"

case "$FORMAT" in
  tar.gz)
    tar -C "$STAGE" -czf "${OUT}/${NAME}.tar.gz" "$BIN" README.md LICENSE
    ARCHIVE="${OUT}/${NAME}.tar.gz"
    ;;
  zip)
    if command -v zip >/dev/null 2>&1; then
      (cd "$STAGE" && zip -qr "${OUT}/${NAME}.zip" "$BIN" README.md LICENSE)
    elif command -v 7z >/dev/null 2>&1; then
      (cd "$STAGE" && 7z a -tzip "${OUT}/${NAME}.zip" "$BIN" README.md LICENSE >/dev/null)
    else
      echo "error: neither zip nor 7z is available for format '$FORMAT'" >&2
      exit 1
    fi
    ARCHIVE="${OUT}/${NAME}.zip"
    ;;
  *)
    echo "error: unsupported format '$FORMAT' (expected tar.gz or zip)" >&2
    exit 2
    ;;
esac

hash_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1"
  else
    shasum -a 256 "$1"
  fi
}

ARCHIVE_NAME="$(basename "$ARCHIVE")"
( cd "$OUT" && hash_file "$ARCHIVE_NAME" > "${ARCHIVE_NAME}.sha256" )

echo "built: ${ARCHIVE}"
cat "${ARCHIVE}.sha256"
