#!/usr/bin/env bash
# Build a runnable AppImage of the native app.
#
#   native/packaging/build-appimage.sh [OUT_DIR]
#
# Bundles the release binary, the icon, the .desktop entry and the public
# data the app reads next to its executable (ecudefs/, database/, ctrldefs/,
# catalog/, packs/ and the guide).
# Needs appimagetool (fetched into ~/.local/bin when missing).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
native="$(dirname "$here")"
root="$(dirname "$native")"
out="${1:-$native/dist}"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$native/Cargo.toml" | head -1)"

(cd "$native" && cargo build --release --locked)
target="$(cd "$native" && cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
bin="$target/release/opentune-native"

app="$(mktemp -d)/OpenTune.AppDir"
mkdir -p "$app/usr/bin" "$app/usr/share/applications" "$app/usr/share/icons/hicolor/256x256/apps" "$app/usr/share/icons/hicolor/512x512/apps"
install -m755 "$bin" "$app/usr/bin/opentune-native"
for d in ecudefs database ctrldefs catalog packs; do
    cp -r "$root/$d" "$app/usr/bin/$d"
done
cp -r "$root/desktop/src-tauri/guide" "$app/usr/bin/guide"
install -m644 "$here/opentune-native.desktop" "$app/usr/share/applications/"
install -m644 "$here/opentune-native.desktop" "$app/opentune-native.desktop"
install -m644 "$native/assets/icon-256.png" "$app/usr/share/icons/hicolor/256x256/apps/opentune-native.png"
install -m644 "$here/opentune.png" "$app/usr/share/icons/hicolor/512x512/apps/opentune-native.png"
install -m644 "$here/opentune.png" "$app/opentune-native.png"
cp "$root/LICENSE" "$app/usr/share/LICENSE" 2>/dev/null || true
cat > "$app/AppRun" <<'EOF'
#!/bin/sh
here="$(dirname "$(readlink -f "$0")")"
exec "$here/usr/bin/opentune-native" "$@"
EOF
chmod 755 "$app/AppRun"

tool="$(command -v appimagetool || true)"
if [ -z "$tool" ]; then
    tool="$HOME/.local/bin/appimagetool"
    if [ ! -x "$tool" ]; then
        mkdir -p "$HOME/.local/bin"
        curl -fsSL -o "$tool" https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage
        chmod +x "$tool"
    fi
fi
mkdir -p "$out"
img="$out/OpenTune-native-$version-x86_64.AppImage"
ARCH=x86_64 APPIMAGE_EXTRACT_AND_RUN=1 "$tool" --no-appstream "$app" "$img" >/dev/null
rm -rf "$(dirname "$app")"
echo "$img"
