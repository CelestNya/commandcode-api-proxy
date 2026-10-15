#!/usr/bin/env bash
# Render the AUR PKGBUILD from the template with the real version/checksum,
# then verify the package builds with a real `makepkg` inside an archlinux
# container.
#
# Usage: verify-aur.sh <version> <target-dir>
# Requires docker on the runner (ubuntu-latest has it).
#
# Outputs (in <target-dir>/packaging-out/):
#   PKGBUILD            - real one, ready to push to
#                         aur.archlinux.org/commandcode-proxy-bin.git
#   .SRCINFO            - generated from the real PKGBUILD
#   commandcode-proxy-bin-<version>-1-x86_64.pkg.tar.zst - build sanity check

set -euo pipefail

VERSION="$1"
ROOT="$2"

OUT_DIR="$ROOT/packaging-out"
TARBALL="$OUT_DIR/commandcode-proxy-$VERSION-linux-x86_64.tar.gz"
SHA=$(sha256sum "$TARBALL" | cut -d' ' -f1)

# --- render the real PKGBUILD (GitHub release URL + real sha256) ---
sed -e "s/@PKGVER@/$VERSION/g" -e "s/@PKGREL@/1/g" -e "s/@SHA256@/$SHA/g" \
    "$ROOT/packaging/aur/PKGBUILD.template" > "$OUT_DIR/PKGBUILD.real"

# --- verification variant: bare local filename (no URL) ---
sed "s#::https://github.com/CelestNya/commandcode-api-proxy/releases/download/v${VERSION}/##" \
    "$OUT_DIR/PKGBUILD.real" > "$OUT_DIR/PKGBUILD.verify"

docker run --rm -v "$OUT_DIR:/out" archlinux:latest bash -euo pipefail -c '
    pacman -Sy --noconfirm --needed base-devel sudo >/dev/null 2>&1
    useradd -m builder
    echo "builder ALL=(ALL) NOPASSWD: ALL" >> /etc/sudoers
    chown -R builder:builder /out
    cd /out
    sudo -u builder bash -c "
        set -euo pipefail
        # verification build resolves the source to the local tarball
        cp PKGBUILD.verify PKGBUILD
        makepkg -f
        # .SRCINFO must describe the REAL package (GitHub URL) for the AUR
        cp PKGBUILD.real PKGBUILD
        makepkg --printsrcinfo > .SRCINFO
    "
    # main pkg only (exclude the *-debug-* variant makepkg also emits)
    tar -tf commandcode-proxy-bin-[0-9]*.pkg.tar.zst | grep -q usr/bin/commandcode-proxy
    echo AUR-VERIFY-OK
'

# The built artifact stays for the record; the real PKGBUILD is the deliverable.
rm -f "$OUT_DIR/PKGBUILD.real" "$OUT_DIR/PKGBUILD.verify"
echo "PKGBUILD + .SRCINFO rendered for commandcode-proxy-bin $VERSION"
