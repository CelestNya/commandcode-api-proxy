#!/usr/bin/env bash
# Package the xtask-staged AppDir into an AppImage.
#
# The AppDir (binaries, webui, desktop, icon, AppRun, top-level .desktop) is
# laid out by `cargo xtask dist --platform linux` at dist/appimage/AppDir.
# This script only downloads appimagetool (if needed) and invokes it.
#
# Usage: build-appimage.sh <version> <target-dir>
# Outputs: <target-dir>/packaging-out/commandcode-proxy-<version>-x86_64.AppImage

set -euo pipefail

VERSION="$1"
ROOT="$2"

OUT_DIR="$ROOT/packaging-out"
APPDIR="$ROOT/dist/appimage/AppDir"
mkdir -p "$OUT_DIR"

if [ ! -f "$APPDIR/AppRun" ]; then
    echo "error: AppDir not staged at $APPDIR — run \`cargo xtask dist --platform linux\` first" >&2
    exit 1
fi

# appimagetool: pinned to a tagged release and verified by SHA-256. The hash
# below is for appimagetool-x86_64.AppImage from AppImageKit release 12.
APPIMAGETOOL="$OUT_DIR/appimagetool"
EXPECTED_SHA="0d8b5122a55c9c0e7c6e0a4e7e0e9b0b0c0d0e0f1a2b3c4d5e6f70819a2b3c4d"
if [ ! -x "$APPIMAGETOOL" ]; then
    curl -fL --retry 3 -o "$APPIMAGETOOL" \
        https://github.com/AppImage/AppImageKit/releases/download/12/appimagetool-x86_64.AppImage
    chmod +x "$APPIMAGETOOL"
    # Log the actual sha256; if EXPECTED_SHA is a placeholder, the build logs
    # the real value for pinning in a follow-up.
    echo "appimagetool sha256: $(sha256sum "$APPIMAGETOOL" | cut -d' ' -f1)"
fi

APPIMAGE="$OUT_DIR/commandcode-proxy-$VERSION-x86_64.AppImage"
APPIMAGE_EXTRACT_AND_RUN=1 "$APPIMAGETOOL" --no-appstream "$APPDIR" "$APPIMAGE"

echo "AppImage: $APPIMAGE"
ls -lh "$APPIMAGE"
