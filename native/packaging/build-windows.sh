#!/usr/bin/env bash
# Cross-compile the native app for Windows x86_64 and lay out a
# self-contained folder + zip:
#
#   native/packaging/build-windows.sh [OUT_DIR]
#
#   OUT_DIR/OpenTune-native-<ver>-windows-x86_64/
#     opentune-native.exe   (statically linked C++ runtime/unwinder; imports
#                            only Windows system DLLs + the Universal CRT)
#     ecudefs/ database/ ctrldefs/   (read from beside the .exe)
#     LICENSE  README.txt
#   OUT_DIR/OpenTune-native-<ver>-windows-x86_64.zip
#
# Toolchain: Rust target x86_64-pc-windows-gnullvm + llvm-mingw
# (https://github.com/mstorsjo/llvm-mingw), found via LLVM_MINGW
# (default ~/opt/llvm-mingw).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
native="$(dirname "$here")"
root="$(dirname "$native")"
out="${1:-$native/dist}"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$native/Cargo.toml" | head -1)"
mingw="${LLVM_MINGW:-$HOME/opt/llvm-mingw}"
t=x86_64-pc-windows-gnullvm

export PATH="$mingw/bin:$PATH"
export CARGO_TARGET_X86_64_PC_WINDOWS_GNULLVM_LINKER=x86_64-w64-mingw32-clang
export CC_x86_64_pc_windows_gnullvm=x86_64-w64-mingw32-clang
export CXX_x86_64_pc_windows_gnullvm=x86_64-w64-mingw32-clang++
export AR_x86_64_pc_windows_gnullvm=llvm-ar
(cd "$native" && cargo build --release --locked --target "$t")
target="$(cd "$native" && cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
exe="$target/$t/release/opentune-native.exe"

# Refuse to ship if the exe needs a DLL Windows does not provide.
extra="$(llvm-objdump -p "$exe" | sed -n 's/.*DLL Name: //p' | grep -viE '^(api-ms-win-|advapi32|bcrypt|bcryptprimitives|cfgmgr32|combase|comctl32|comdlg32|d3d|dwmapi|dxgi|gdi32|hid|imm32|kernel32|ntdll|ole32|oleaut32|opengl32|propsys|setupapi|shell32|shlwapi|user32|uxtheme|winmm|ws2_32|userenv|version|crypt32|secur32|ncrypt|dbghelp|powrprof|winhttp|iphlpapi|psapi)' || true)"
if [ -n "$extra" ]; then
    echo "non-system DLL imports, not bundled: $extra" >&2
    exit 1
fi

name="OpenTune-native-$version-windows-x86_64"
dir="$out/$name"
rm -rf "$dir" "$out/$name.zip"
mkdir -p "$dir"
cp "$exe" "$dir/"
for d in ecudefs database ctrldefs; do
    cp -r "$root/$d" "$dir/$d"
done
cp "$root/LICENSE" "$dir/LICENSE" 2>/dev/null || true
cat > "$dir/README.txt" <<EOF
OpenTune native $version (preview) - Windows x86_64

Run opentune-native.exe. Keep the ecudefs, database and ctrldefs folders next
to it. Optional: pass a dump path as the first argument.
Review/measurement modes: opentune-native.exe --help
GPL-3.0-or-later. https://github.com/ebjerk01/opentune
EOF
(cd "$out" && python3 -c "import shutil,sys; shutil.make_archive(sys.argv[1], 'zip', '.', sys.argv[1])" "$name")
echo "$dir"
echo "$out/$name.zip"
