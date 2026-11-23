#!/usr/bin/env bash
# Build the GitHub release asset for a lyrachi tag.
#
# Usage: packaging/build-release-asset.sh [tag]
# Output: lyrachi-v<version>-x86_64-unknown-linux-gnu.tar.gz plus .sha256
# in the current directory.
#
# The tarball contains the release binary, README.md, and LICENSE, which is
# the layout packaging/aur/lyrachi-bin/PKGBUILD expects.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

TAG="${1:-$(git describe --tags --abbrev=0)}"
VERSION="${TAG#v}"
OUT="lyrachi-v${VERSION}-x86_64-unknown-linux-gnu"

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

export CARGO_INCREMENTAL=0
cargo build --release --locked

install -m0755 target/release/lyrachi "$STAGE/lyrachi"
install -m0644 README.md "$STAGE/README.md"
install -m0644 LICENSE "$STAGE/LICENSE"

tar -C "$STAGE" -czf "$OUT.tar.gz" lyrachi README.md LICENSE
sha256sum "$OUT.tar.gz" > "$OUT.tar.gz.sha256"

echo "built: $OUT.tar.gz"
cat "$OUT.tar.gz.sha256"
