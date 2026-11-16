#!/usr/bin/env bash
# Sync the public root ECU definitions into the Android app's bundled assets.
#
#   <repo>/ecudefs/*.json  ->  android/app/src/main/assets/ecudefs/*.json
#
# The Android app bundles a copy of the ECU defs as APK assets (it has no
# filesystem access to the repo at runtime). That copy MUST stay identical to
# the canonical root `ecudefs/` set, otherwise the app ships stale or missing
# definitions. Run this after changing any root def; CI enforces it via the
# `--check` mode below (see android/app build config / check-ecudefs).
#
# Scaffolding is skipped: TEMPLATE.json, anything starting with `_`, and dotfiles.
#
# Usage:
#   android/sync-ecudefs.sh           # copy root -> assets (default)
#   android/sync-ecudefs.sh --check   # verify they match; non-zero exit on drift
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/.." && pwd)"
SRC="$REPO/ecudefs"
DST="$HERE/app/src/main/assets/ecudefs"

MODE="sync"
if [[ "${1:-}" == "--check" ]]; then
    MODE="check"
elif [[ -n "${1:-}" ]]; then
    echo "usage: $(basename "$0") [--check]" >&2
    exit 2
fi

# List the canonical def filenames (basenames), skipping scaffolding.
list_defs() {
    for f in "$SRC"/*.json; do
        [[ -e "$f" ]] || continue
        local b
        b="$(basename "$f")"
        case "$b" in
            TEMPLATE.json|_*|.*) continue ;;
        esac
        printf '%s\n' "$b"
    done
}

if [[ "$MODE" == "check" ]]; then
    status=0
    # Every canonical def must exist in assets and be byte-identical.
    while IFS= read -r b; do
        if [[ ! -f "$DST/$b" ]]; then
            echo "DRIFT: missing in assets: $b" >&2
            status=1
        elif ! cmp -s "$SRC/$b" "$DST/$b"; then
            echo "DRIFT: content differs: $b" >&2
            status=1
        fi
    done < <(list_defs)
    # No stray defs in assets that are not canonical.
    for f in "$DST"/*.json; do
        [[ -e "$f" ]] || continue
        b="$(basename "$f")"
        if [[ ! -f "$SRC/$b" ]]; then
            echo "DRIFT: stray asset not in root ecudefs: $b" >&2
            status=1
        fi
    done
    if [[ "$status" -eq 0 ]]; then
        echo "ecudefs assets are in sync with root ecudefs/."
    else
        echo "Run android/sync-ecudefs.sh to fix." >&2
    fi
    exit "$status"
fi

# sync mode
mkdir -p "$DST"
# Remove any stray (non-canonical) asset defs first so deletions propagate.
for f in "$DST"/*.json; do
    [[ -e "$f" ]] || continue
    b="$(basename "$f")"
    if [[ ! -f "$SRC/$b" ]]; then
        echo "rm (stray): $b"
        rm -f "$f"
    fi
done
count=0
while IFS= read -r b; do
    cp -f "$SRC/$b" "$DST/$b"
    echo "sync: $b"
    count=$((count + 1))
done < <(list_defs)
echo ">> Synced $count ECU def(s) into $DST"
