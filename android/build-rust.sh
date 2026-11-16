#!/usr/bin/env bash
# Build the OpenTune Rust core (opentune-ffi) for Android and regenerate the
# Kotlin bindings. Produces, for each ABI:
#
#   app/src/main/jniLibs/<abi>/libopentune_ffi.so
#
# and refreshes:
#
#   app/src/main/java/org/opentune/ffi/opentune.kt
#
# PREREQUISITES (none of which may be installed on a plain Rust box):
#   - Android SDK + NDK (set ANDROID_NDK_HOME or install via sdkmanager)
#   - cargo-ndk:         cargo install cargo-ndk
#   - Rust std for each target:
#       rustup target add aarch64-linux-android armv7-linux-androideabi \
#                         i686-linux-android x86_64-linux-android
#
# Run from the `android/` directory:  ./build-rust.sh
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/.." && pwd)"
FFI_MANIFEST="$REPO/crates/opentune-ffi/Cargo.toml"
JNILIBS="$HERE/app/src/main/jniLibs"
KT_OUT="$HERE/app/src/main/java"
UNIFFI_CONFIG="$REPO/crates/opentune-ffi/uniffi.toml"

# Android ABI -> Rust target (for reference / manual builds).
#   arm64-v8a    -> aarch64-linux-android
#   armeabi-v7a  -> armv7-linux-androideabi
#   x86_64       -> x86_64-linux-android
#   x86          -> i686-linux-android

command -v cargo-ndk >/dev/null 2>&1 || {
    echo "ERROR: cargo-ndk not found. Install it with:  cargo install cargo-ndk" >&2
    exit 1
}

echo ">> Building libopentune_ffi.so for 4 ABIs into $JNILIBS"
cargo ndk \
    -t arm64-v8a \
    -t armeabi-v7a \
    -t x86_64 \
    -t x86 \
    -o "$JNILIBS" \
    build --release --manifest-path "$FFI_MANIFEST"

echo ">> Regenerating Kotlin bindings into $KT_OUT"
# Library mode: generate from one freshly built .so (any ABI will do; the API
# surface is identical). Use the host build for speed if present, else arm64.
HOST_SO="${CARGO_TARGET_DIR:-$REPO/target}/release/libopentune_ffi.so"
if [[ -f "$HOST_SO" ]]; then
    LIB="$HOST_SO"
else
    LIB="$JNILIBS/arm64-v8a/libopentune_ffi.so"
fi

cargo run --manifest-path "$FFI_MANIFEST" --bin uniffi-bindgen -- \
    generate \
    --library "$LIB" \
    --language kotlin \
    --config "$UNIFFI_CONFIG" \
    --out-dir "$KT_OUT"

echo ">> Done. Build the APK with:  ./gradlew assembleDebug"
