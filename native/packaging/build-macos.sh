#!/usr/bin/env bash
# Build OpenTune.app on a Mac (run ON macOS; Apple's SDK is not available to
# cross-compile from Linux):
#
#   native/packaging/build-macos.sh [OUT_DIR]
#
#   OUT_DIR/OpenTune.app/Contents/
#     Info.plist
#     MacOS/opentune-native
#     Resources/{ecudefs,database,ctrldefs,catalog,packs,guide}/ + opentune.icns
#   OUT_DIR/OpenTune-native-<ver>-macos-<arch>.zip
#
# Needs: Xcode command-line tools (xcode-select --install) and rustup.
# The bundle is ad-hoc signed (required to run on Apple Silicon); it is not
# notarised, so a copy downloaded from elsewhere needs right-click > Open once.
set -euo pipefail
[ "$(uname -s)" = Darwin ] || { echo "run this on macOS" >&2; exit 1; }
here="$(cd "$(dirname "$0")" && pwd)"
native="$(dirname "$here")"
root="$(dirname "$native")"
out="${1:-$native/dist}"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$native/Cargo.toml" | head -1)"
arch="$(uname -m)"

(cd "$native" && cargo build --release --locked)
target="$(cd "$native" && cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"

app="$out/OpenTune.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
install -m755 "$target/release/opentune-native" "$app/Contents/MacOS/opentune-native"
for d in ecudefs database ctrldefs catalog packs; do
    cp -R "$root/$d" "$app/Contents/Resources/$d"
done
cp -R "$root/desktop/src-tauri/guide" "$app/Contents/Resources/guide"

# Icon: an .iconset from the 512 px master, then iconutil.
set_dir="$(mktemp -d)/opentune.iconset"
mkdir -p "$set_dir"
for s in 16 32 128 256 512; do
    sips -z $s $s "$here/opentune.png" --out "$set_dir/icon_${s}x${s}.png" >/dev/null
    d=$((s * 2))
    [ $d -le 512 ] && sips -z $d $d "$here/opentune.png" --out "$set_dir/icon_${s}x${s}@2x.png" >/dev/null
done
cp "$here/opentune.png" "$set_dir/icon_256x256@2x.png"
iconutil -c icns "$set_dir" -o "$app/Contents/Resources/opentune.icns"
rm -rf "$(dirname "$set_dir")"

cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>OpenTune</string>
  <key>CFBundleDisplayName</key><string>OpenTune</string>
  <key>CFBundleIdentifier</key><string>io.github.ebjerk01.opentune.native</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleExecutable</key><string>opentune-native</string>
  <key>CFBundleIconFile</key><string>opentune.icns</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
</dict>
</plist>
EOF

codesign --force --deep --sign - "$app"
zip="$out/OpenTune-native-$version-macos-$arch.zip"
rm -f "$zip"
ditto -c -k --keepParent "$app" "$zip"
echo "$app"
echo "$zip"
