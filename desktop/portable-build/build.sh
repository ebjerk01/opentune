#!/usr/bin/env bash
#
# Build a portable OpenTune desktop AppImage (+ .deb) against an old glibc.
#
# Run it from the repo root on a machine with Docker:
#
#     desktop/portable-build/build.sh
#
# It builds the debian:bookworm image (glibc 2.36 baseline), compiles the
# desktop app inside it, and leaves the bundles in ./dist/.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
IMAGE=opentune-portable-builder

echo "### building the bookworm (glibc 2.36) build image"
docker build -t "$IMAGE" "$REPO_ROOT/desktop/portable-build"

echo "### compiling the desktop app inside the container"
mkdir -p "$REPO_ROOT/dist" "$REPO_ROOT/.portable-target"
docker run --rm \
  -v "$REPO_ROOT:/src:ro" \
  -v "$REPO_ROOT/.portable-target:/build/target" \
  -v "$REPO_ROOT/dist:/dist" \
  -e CARGO_TARGET_DIR=/build/target \
  -e APPIMAGE_EXTRACT_AND_RUN=1 \
  "$IMAGE" bash -c '
    set -euo pipefail
    cd /src/desktop
    # Local builds are unsigned: --no-sign skips the updater signing step (which
    # needs the private key that only CI holds). Signed releases come from
    # .github/workflows/release.yml.
    cargo tauri build --bundles appimage,deb --no-sign
    mkdir -p /dist
    find /build/target -type f \( -name "*.AppImage" -o -name "*.deb" \) -exec cp -v {} /dist/ \;
    ls /dist/*.AppImage >/dev/null 2>&1 || { echo "ERROR: no AppImage was produced"; exit 1; }
  '

echo "### done — portable bundles:"
ls -la "$REPO_ROOT/dist"
echo "### glibc baseline: 2.36 (Debian 12). Verify with:"
echo "###   ./dist/*.AppImage --appimage-extract && objdump -T squashfs-root/usr/bin/* | grep -oE 'GLIBC_[0-9.]+' | sort -V | tail -1"
