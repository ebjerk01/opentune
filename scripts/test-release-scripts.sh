#!/usr/bin/env bash
# Self-test for the release helpers (no cargo, no network, no keys):
#   scripts/bump-version.sh     - set/check on a scratch copy of the version files
#   scripts/make-latest-json.py - manifest layout + signature verification, using
#                                 the PUBLIC minisign test vector from the
#                                 minisign-verify crate (signature over "test").
#
#   scripts/test-release-scripts.sh            # PYTHON=python3 by default
#   PYTHON=/path/venv/bin/python scripts/test-release-scripts.sh
#
# The signature checks need python `cryptography`; without it they are skipped
# (with a notice) unless REQUIRE_CRYPTO=1.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PY="${PYTHON:-python3}"
GEN="$REPO_ROOT/scripts/make-latest-json.py"
T="$(mktemp -d)"
trap 'rm -rf "$T"' EXIT
fail() { echo "FAIL: $*" >&2; exit 1; }
ok() { echo "ok   $*"; }

# --- bump-version.sh ----------------------------------------------------------
"$REPO_ROOT/scripts/bump-version.sh" --check >/dev/null || fail "repo versions disagree"
ok "repo version fields agree ($("$REPO_ROOT/scripts/bump-version.sh" --print))"

(cd "$REPO_ROOT" && git ls-files -- '*Cargo.toml' '*Cargo.lock' desktop/src-tauri/tauri.conf.json \
    desktop/package.json) | while read -r f; do
    mkdir -p "$T/repo/$(dirname "$f")"
    cp "$REPO_ROOT/$f" "$T/repo/$f"
done
mkdir -p "$T/repo/scripts" && cp "$REPO_ROOT/scripts/bump-version.sh" "$T/repo/scripts/"
"$T/repo/scripts/bump-version.sh" 7.8.9-rc.1 >/dev/null || fail "bump to 7.8.9-rc.1"
"$T/repo/scripts/bump-version.sh" --check v7.8.9-rc.1 >/dev/null || fail "check after bump"
[ "$("$T/repo/scripts/bump-version.sh" --print)" = 7.8.9-rc.1 ] || fail "--print after bump"
"$PY" -I -c 'import json,sys; json.load(open(sys.argv[1])); json.load(open(sys.argv[2]))' \
    "$T/repo/desktop/src-tauri/tauri.conf.json" "$T/repo/desktop/package.json" || fail "JSON broken"
ok "bump-version set + check on a scratch copy"
if "$T/repo/scripts/bump-version.sh" --check v7.8.10 >/dev/null 2>&1; then fail "tag mismatch accepted"; fi
ok "bump-version rejects a tag mismatch"
for bad in 1.2 1.2.3+build 01.2.3 1.2.3-; do
    if "$T/repo/scripts/bump-version.sh" "$bad" >/dev/null 2>&1; then fail "accepted '$bad'"; fi
done
ok "bump-version rejects malformed versions"

# --- make-latest-json.py ------------------------------------------------------
# Public test vector (minisign-verify 0.3.0, `verify_prehashed`): NOT a project key.
PUB=RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3
SIG=$(printf 'untrusted comment: signature from minisign secret key\nRUQf6LRCGA9i559r3g7V1qNyJDApGip8MfqcadIgT9CuhV3EMhHoN1mGTkUidF/z7SrlQgXdy8ofjb7bNJJylDOocrCo8KLzZwo=\ntrusted comment: timestamp:1556193335\tfile:test\ny/rUw2y8/hOUYjZU71eHp/Wo1KZ40fGy2VJEDl34XMJM+TX48Ss/17u3IvIfbVR1FkZZSNCisQbuQY+bHwhEBg==\n' | base64 | tr -d '\n')
cat >"$T/conf.json" <<EOF
{"version": "9.9.9", "plugins": {"updater": {
  "endpoints": ["https://github.com/o/r/releases/latest/download/latest.json"],
  "pubkey": "$PUB"}}}
EOF
A="$T/assets"
mkdir -p "$A"
for n in OpenTune_9.9.9_amd64.AppImage OpenTune_9.9.9_amd64.deb OpenTune_9.9.9_x64-setup.exe \
    OpenTune_9.9.9_x64_en-US.msi; do
    printf test >"$A/$n"
    printf '%s' "$SIG" >"$A/$n.sig"
done
printf 'not a bundle' >"$A/opentune-v9.9.9-x86_64-unknown-linux-gnu.tar.gz"
gen() { "$PY" -I "$GEN" --conf "$T/conf.json" --tag v9.9.9 --repo o/r --pub-date 2026-01-01T00:00:00Z "$@"; }

gen --assets "$A" --no-verify --out "$T/latest.json" 2>/dev/null || fail "generator (no-verify)"
"$PY" -I - "$T/latest.json" <<'PY' || fail "latest.json layout"
import json, sys
m = json.load(open(sys.argv[1]))
assert m["version"] == "9.9.9" and m["pub_date"] == "2026-01-01T00:00:00Z", m
p = m["platforms"]
base = "https://github.com/o/r/releases/download/v9.9.9/"
want = {
    "linux-x86_64": "OpenTune_9.9.9_amd64.AppImage",
    "linux-x86_64-appimage": "OpenTune_9.9.9_amd64.AppImage",
    "linux-x86_64-deb": "OpenTune_9.9.9_amd64.deb",
    "windows-x86_64": "OpenTune_9.9.9_x64-setup.exe",
    "windows-x86_64-nsis": "OpenTune_9.9.9_x64-setup.exe",
    "windows-x86_64-msi": "OpenTune_9.9.9_x64_en-US.msi",
}
assert set(p) == set(want), sorted(p)
for k, v in want.items():
    assert p[k]["url"] == base + v, (k, p[k]["url"])
    assert p[k]["signature"], k
PY
ok "latest.json platform keys + URLs"

mkdir -p "$T/stale" && printf test >"$T/stale/OpenTune_9.9.8_amd64.AppImage" && printf '%s' "$SIG" >"$T/stale/OpenTune_9.9.8_amd64.AppImage.sig"
if gen --assets "$T/stale" --no-verify >/dev/null 2>&1; then fail "stale-version bundle accepted"; fi
ok "rejects a bundle from another version"
mkdir -p "$T/nosig" && printf test >"$T/nosig/OpenTune_9.9.9_amd64.AppImage"
if gen --assets "$T/nosig" --no-verify >/dev/null 2>&1; then fail "bundle without .sig accepted"; fi
ok "rejects a bundle without .sig"
if gen --check-config --repo someone/fork 2>/dev/null; then fail "foreign endpoint accepted"; fi
ok "rejects an endpoint for another repository"

if "$PY" -I -c 'import cryptography' 2>/dev/null; then
    gen --assets "$A" --require-verify >/dev/null 2>&1 || fail "valid signatures rejected"
    ok "verifies real minisign signatures (prehashed + global)"
    printf Test >"$A/OpenTune_9.9.9_x64-setup.exe"
    if gen --assets "$A" --require-verify >/dev/null 2>&1; then fail "tampered bundle accepted"; fi
    ok "rejects a tampered bundle"
elif [ "${REQUIRE_CRYPTO:-0}" = 1 ]; then
    fail "python cryptography not importable (REQUIRE_CRYPTO=1)"
else
    echo "skip signature verification tests (no python cryptography)"
fi
# Key-id check works without cryptography: the project's real pubkey must refuse
# a signature made by any other key.
mkdir -p "$T/other" && printf test >"$T/other/OpenTune_0.0.0_amd64.AppImage" && printf '%s' "$SIG" >"$T/other/OpenTune_0.0.0_amd64.AppImage.sig"
"$PY" -I - "$REPO_ROOT/desktop/src-tauri/tauri.conf.json" "$T/conf2.json" <<'PY'
import json, sys
c = json.load(open(sys.argv[1])); c["version"] = "0.0.0"
c["plugins"]["updater"]["endpoints"] = ["https://github.com/o/r/releases/latest/download/latest.json"]
json.dump(c, open(sys.argv[2], "w"))
PY
if "$PY" -I "$GEN" --conf "$T/conf2.json" --tag v0.0.0 --repo o/r --assets "$T/other" >/dev/null 2>&1; then
    fail "signature from a foreign key accepted"
fi
ok "rejects a signature from a key the app does not trust"
echo "release scripts: all tests passed"
