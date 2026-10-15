#!/usr/bin/env bash
# Assemble the AUR source tarball for commandcode-proxy-bin.
#
# The AUR package is precompiled ("-bin", no kernel modules so no rebuild):
# source = this tarball from the GitHub release, package() just unpacks it.
# Layout matches PKGBUILD's package() install lines.
#
# Usage: build-tarball.sh <version> <target-dir>
# Outputs: <target-dir>/packaging-out/commandcode-proxy-<version>-linux-x86_64.tar.gz

set -euo pipefail

VERSION="$1"
ROOT="$2"

OUT_DIR="$ROOT/packaging-out"
STAGE="$OUT_DIR/tarball-stage"
rm -rf "$STAGE"
mkdir -p "$STAGE/usr/bin" "$STAGE/usr/share/commandcode-proxy/webui" \
    "$STAGE/usr/lib/systemd/system" "$STAGE/usr/share/applications" \
    "$STAGE/usr/share/icons/hicolor/scalable/apps" "$STAGE/etc/commandcode-proxy"

install -Dm755 "$ROOT/target/release/ccproxy"      "$STAGE/usr/bin/commandcode-proxy"
install -Dm755 "$ROOT/target/release/CCProxyTray"  "$STAGE/usr/bin/commandcode-proxy-tray"
cp -r "$ROOT/crates/ccproxy/webui/." "$STAGE/usr/share/commandcode-proxy/webui/"
install -Dm644 "$ROOT/packaging/systemd/commandcode-proxy.service" \
    "$STAGE/usr/lib/systemd/system/commandcode-proxy.service"
install -Dm644 "$ROOT/packaging/desktop/commandcode-proxy-tray.desktop" \
    "$STAGE/usr/share/applications/commandcode-proxy-tray.desktop"
install -Dm644 "$ROOT/packaging/icons/commandcode-proxy.svg" \
    "$STAGE/usr/share/icons/hicolor/scalable/apps/commandcode-proxy.svg"
install -Dm640 "$ROOT/packaging/deb/env.example" \
    "$STAGE/etc/commandcode-proxy/env.example"

TARBALL="$OUT_DIR/commandcode-proxy-$VERSION-linux-x86_64.tar.gz"
tar -C "$STAGE" -czf "$TARBALL" .

echo "Tarball: $TARBALL"
sha256sum "$TARBALL"
