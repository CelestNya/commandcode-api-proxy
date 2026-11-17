#!/usr/bin/env bash
# Copy the xtask-produced Linux tarball into packaging-out/ where verify-aur.sh
# expects it. The FHS tree and tar.gz are built by `cargo xtask dist --platform
# linux`; this script no longer stages files itself.
#
# Usage: build-tarball.sh <version> <target-dir>
# Outputs: <target-dir>/packaging-out/commandcode-proxy-<version>-linux-x86_64.tar.gz

set -euo pipefail

VERSION="$1"
ROOT="$2"

OUT_DIR="$ROOT/packaging-out"
mkdir -p "$OUT_DIR"

SRC="$ROOT/dist/commandcode-proxy-v${VERSION}-linux-x86_64.tar.gz"
DST="$OUT_DIR/commandcode-proxy-${VERSION}-linux-x86_64.tar.gz"

if [ ! -f "$SRC" ]; then
    echo "error: xtask tarball not found at $SRC — run \`cargo xtask dist --platform linux\` first" >&2
    exit 1
fi
cp "$SRC" "$DST"

echo "Tarball: $DST"
sha256sum "$DST"
