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

# --- run the verification build inside an archlinux container ---
# The container script is written to a temp file and bind-mounted in, instead
# of passed via `bash -c '...'`. The previous three-level quote nesting
# (single-quoted docker arg + double-quoted sudo -u bash -c) broke as soon as
# we added comments or extra lines. A quoted heredoc (`<<'CONTAINER_SCRIPT'`)
# does no expansion at any layer, and `sudo -u builder bash -s` reads its
# script from a nested heredoc, so there are no quotes to balance.
SCRIPT="$(mktemp)"
trap 'rm -f "$SCRIPT"' EXIT

cat > "$SCRIPT" <<'CONTAINER_SCRIPT'
set -euo pipefail
pacman -Sy --noconfirm --needed base-devel sudo >/dev/null 2>&1
useradd -m builder
echo "builder ALL=(ALL) NOPASSWD: ALL" >> /etc/sudoers
chown -R builder:builder /out
cd /out

# Run the makepkg steps as builder via bash -s (script on stdin).
# Note: archlinux:latest ships a fakeroot that prints
# "libfakeroot internal error: payload not recognized!" on some runners;
# makepkg continues and produces a valid pkg.tar.zst. Treated as known noise.
sudo -u builder bash -s <<'BUILDER_SCRIPT'
set -euo pipefail
cp PKGBUILD.verify PKGBUILD
makepkg -f
# .SRCINFO must describe the REAL package (GitHub URL) for the AUR.
cp PKGBUILD.real PKGBUILD
makepkg --printsrcinfo > .SRCINFO
# Non-dot copy: upload-artifact and action-gh-release skip dotfiles, and
# /out is chowned to builder so the host runner cannot make this sibling.
cp .SRCINFO SRCINFO
BUILDER_SCRIPT

# main pkg only (exclude the *-debug-* variant makepkg also emits);
# no -q: grep must drain the pipe or tar dies with SIGPIPE (141).
tar -tf commandcode-proxy-bin-[0-9]*.pkg.tar.zst | grep usr/bin/commandcode-proxy >/dev/null
# Drop intermediates while we still own /out (docker chowned everything to
# builder; the host runner cannot rm them afterwards).
rm -f PKGBUILD.real PKGBUILD.verify *-debug-*.pkg.tar.zst
rm -rf pkg
test -s .SRCINFO && test -s SRCINFO
ls -la .SRCINFO SRCINFO PKGBUILD
echo AUR-VERIFY-OK
CONTAINER_SCRIPT

docker run --rm -v "$OUT_DIR:/out" -v "$SCRIPT:/verify.sh:ro" \
    archlinux:latest bash /verify.sh

# The built artifact stays for the record; the real PKGBUILD is the deliverable
# (variants already removed inside the container).
echo "PKGBUILD + .SRCINFO rendered for commandcode-proxy-bin $VERSION"
