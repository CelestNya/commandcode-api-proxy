#!/usr/bin/env bash
# Assemble a commandcode-proxy AppImage.
#
# Runs on the release Linux job (ubuntu-latest). The proxy and tray are pure
# Rust with no GTK/Qt runtime deps (SNI goes through zbus), so the AppDir is
# just binaries + webui + desktop file + icon; no linuxdeploy plugin needed.
#
# Usage: build-appimage.sh <version> <target-dir>
#   version    - e.g. 0.7.0 (used for the output file name)
#   target-dir - where target/release lives (repo root on the runner)
#
# Outputs:
#   <target-dir>/packaging-out/commandcode-proxy-<version>-x86_64.AppImage

set -euo pipefail

VERSION="$1"
ROOT="$2"

OUT_DIR="$ROOT/packaging-out"
APPDIR="$OUT_DIR/AppDir"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/commandcode-proxy/webui" \
    "$APPDIR/usr/share/applications" "$APPDIR/usr/share/icons/hicolor/scalable/apps"

# --- binaries (release name; crate stays ccproxy) ---
install -Dm755 "$ROOT/target/release/ccproxy"      "$APPDIR/usr/bin/commandcode-proxy"
install -Dm755 "$ROOT/target/release/CCProxyTray"  "$APPDIR/usr/bin/commandcode-proxy-tray"

# --- webui (external front-end, single source of truth) ---
cp -r "$ROOT/crates/ccproxy/webui/." "$APPDIR/usr/share/commandcode-proxy/webui/"

# --- desktop entry + icon ---
install -Dm644 "$ROOT/packaging/desktop/commandcode-proxy-tray.desktop" \
    "$APPDIR/usr/share/applications/commandcode-proxy-tray.desktop"
install -Dm644 "$ROOT/packaging/icons/commandcode-proxy.svg" \
    "$APPDIR/usr/share/icons/hicolor/scalable/apps/commandcode-proxy.svg"
# Icon must be next to the desktop file for appimagetool detection.
cp "$ROOT/packaging/icons/commandcode-proxy.svg" "$APPDIR/commandcode-proxy.svg"

# --- AppRun: point env at the AppDir before exec'ing the CLI ---
cat > "$APPDIR/AppRun" <<'EOF'
#!/bin/sh
# commandcode-proxy AppImage entry point.
# APPDIR is exported by the AppImage runtime; env stays scoped to this exec.
export CC_WEBUI_DIR="$APPDIR/usr/share/commandcode-proxy/webui"
exec "$APPDIR/usr/bin/commandcode-proxy" "$@"
EOF
chmod +x "$APPDIR/AppRun"

# --- top-level desktop file for appimagetool (same as installed one) ---
cat > "$APPDIR/commandcode-proxy.desktop" <<'EOF'
[Desktop Entry]
Type=Application
Name=commandcode-proxy
Comment=commandcode.ai API translation proxy
Exec=commandcode-proxy
Icon=commandcode-proxy
Terminal=true
Categories=Network;
EOF

# --- appimagetool (self-extracting AppImage; APPIMAGE_EXTRACT_AND_RUN for
# runners without FUSE) ---
APPIMAGETOOL="$OUT_DIR/appimagetool"
if [ ! -x "$APPIMAGETOOL" ]; then
    curl -fL --retry 3 -o "$APPIMAGETOOL" \
        https://github.com/AppImage/AppImageKit/releases/download/continuous/appimagetool-x86_64.AppImage
    chmod +x "$APPIMAGETOOL"
fi

APPIMAGE="$OUT_DIR/commandcode-proxy-$VERSION-x86_64.AppImage"
APPIMAGE_EXTRACT_AND_RUN=1 "$APPIMAGETOOL" --no-appstream "$APPDIR" "$APPIMAGE"

echo "AppImage: $APPIMAGE"
ls -lh "$APPIMAGE"
