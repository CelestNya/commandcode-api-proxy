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

# appimagetool: pinned to AppImageKit release 12 and verified by SHA-256.
# The hash below is for appimagetool-x86_64.AppImage v12 (1,974,248 bytes,
# pinned from a real download). A mismatch fails the build non-zero — we do
# not silently re-vendor a different toolchain.
APPIMAGETOOL="$OUT_DIR/appimagetool"
EXPECTED_SHA="d918b4df547b388ef253f3c9e7f6529ca81a885395c31f619d9aaf7030499a13"
if [ ! -x "$APPIMAGETOOL" ]; then
    curl -fL --retry 3 -o "$APPIMAGETOOL" \
        https://github.com/AppImage/AppImageKit/releases/download/12/appimagetool-x86_64.AppImage
    chmod +x "$APPIMAGETOOL"
fi
ACTUAL_SHA="$(sha256sum "$APPIMAGETOOL" | cut -d' ' -f1)"
if [ "$ACTUAL_SHA" != "$EXPECTED_SHA" ]; then
    echo "error: appimagetool sha256 mismatch" >&2
    echo "  expected: $EXPECTED_SHA" >&2
    echo "  actual:   $ACTUAL_SHA" >&2
    exit 1
fi
echo "appimagetool sha256 OK: $ACTUAL_SHA"

APPIMAGE="$OUT_DIR/commandcode-proxy-$VERSION-x86_64.AppImage"
APPIMAGE_EXTRACT_AND_RUN=1 "$APPIMAGETOOL" --no-appstream "$APPDIR" "$APPIMAGE"

echo "AppImage: $APPIMAGE"
ls -lh "$APPIMAGE"

# Post-build assertions: a non-empty AppImage is not enough. Verify the file
# exists, is large enough to have squashfs'd real content, and actually
# contains the anchor files we ship. We list the squashfs by running the
# produced AppImage with --appimage-list (supported by runtime v2); fall back
# to unsquashfs -l if present.
test -s "$APPIMAGE" || { echo "error: AppImage is empty" >&2; exit 1; }
SIZE="$(stat -c%s "$APPIMAGE")"
if [ "$SIZE" -lt 5000000 ]; then
    echo "error: AppImage too small ($SIZE bytes); expected >5MB" >&2
    exit 1
fi

LIST_TMP="$(mktemp)"
if "$APPIMAGE" --appimage-list > "$LIST_TMP" 2>/dev/null; then
    : # runtime list worked
elif command -v unsquashfs >/dev/null 2>&1; then
    unsquashfs -l "$APPIMAGE" > "$LIST_TMP"
else
    echo "error: neither --appimage-list nor unsquashfs available to inspect AppImage" >&2
    rm -f "$LIST_TMP"
    exit 1
fi

need_anchor() {
    needle="$1"
    count="$(grep -c -F "$needle" "$LIST_TMP" || true)"
    if [ "$count" != "1" ]; then
        echo "error: AppImage missing anchor '$needle' (matches=$count)" >&2
        echo "--- listing ---" >&2
        cat "$LIST_TMP" >&2
        rm -f "$LIST_TMP"
        exit 1
    fi
}
need_anchor "usr/bin/commandcode-proxy"
need_anchor "usr/bin/commandcode-proxy-tray"
need_anchor "usr/share/commandcode-proxy/webui/index.html"
need_anchor ".DirIcon"
rm -f "$LIST_TMP"
echo "AppImage anchors OK"
