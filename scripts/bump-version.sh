#!/usr/bin/env bash
# Set, check or print the OpenTune release version (one source of truth).
#
#   scripts/bump-version.sh 0.4.0            # set every version field to 0.4.0
#   scripts/bump-version.sh 0.4.0-alpha.1    # pre-release (see docs/RELEASING.md)
#   scripts/bump-version.sh --check          # all fields agree?          (CI)
#   scripts/bump-version.sh --check v0.4.0   # ...and equal this tag?     (release CI)
#   scripts/bump-version.sh --print          # print the current version
#
# The release version lives in these places, which must always agree:
#   Cargo.toml                       [workspace.package] version
#                                    (every crate with `version.workspace = true`,
#                                    incl. the `opentune` CLI, inherits it)
#   desktop/src-tauri/Cargo.toml     [package] version  (desktop crate is not a
#                                    workspace member, so it has its own field)
#   desktop/src-tauri/tauri.conf.json  "version"  (what the updater compares)
#   desktop/package.json             "version"
#   Cargo.lock, desktop/src-tauri/Cargo.lock  entries of the packages above
#
# Crates that pin their own `version = "..."` are independent library versions
# and are left alone. The Android app (android/app/build.gradle.kts) is
# released separately and is not touched either.
#
# Accepted versions: MAJOR.MINOR.PATCH with an optional `-pre.release` suffix
# (semver). Build metadata (`+...`) is rejected: GitHub rewrites `+` in asset
# names, which would break the updater URLs in latest.json.
#
# Needs only python3 (no cargo, no network). Works from any directory.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

case "${1:-}" in
    -h | --help | "")
        sed -n '2,27p' "$0" | sed 's/^# \{0,1\}//'
        [ -n "${1:-}" ] && exit 0 || exit 2
        ;;
esac

exec python3 -I - "$REPO_ROOT" "$@" <<'PY'
import json
import os
import re
import sys

root = sys.argv[1]
args = sys.argv[2:]

SEMVER = re.compile(
    r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)"
    r"(?:-((?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*)"
    r"(?:\.(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*))*))?$"
)

WS_TOML = os.path.join(root, "Cargo.toml")
DESK_TOML = os.path.join(root, "desktop", "src-tauri", "Cargo.toml")
TAURI_CONF = os.path.join(root, "desktop", "src-tauri", "tauri.conf.json")
PKG_JSON = os.path.join(root, "desktop", "package.json")
WS_LOCK = os.path.join(root, "Cargo.lock")
DESK_LOCK = os.path.join(root, "desktop", "src-tauri", "Cargo.lock")


def die(msg, code=1):
    print(f"bump-version: {msg}", file=sys.stderr)
    sys.exit(code)


def read(path):
    with open(path, encoding="utf-8") as f:
        return f.read()


def write(path, text):
    with open(path, "w", encoding="utf-8", newline="\n") as f:
        f.write(text)


def rel(path):
    return os.path.relpath(path, root)


# --- TOML: the `version = "..."` line of one [section] ------------------------
def toml_section_version(text, section):
    """Return (match, value) for `version = "x"` inside [section]."""
    m = re.search(r"(?m)^\[" + re.escape(section) + r"\]\s*$", text)
    if not m:
        return None, None
    start = m.end()
    nxt = re.search(r"(?m)^\[", text[start:])
    end = start + nxt.start() if nxt else len(text)
    v = re.compile(r'(?m)^(version\s*=\s*")([^"]*)(")').search(text, start, end)
    if not v:
        return None, None
    return v, v.group(2)


def toml_set_section_version(path, section, new):
    text = read(path)
    m, _ = toml_section_version(text, section)
    if not m:
        die(f"{rel(path)}: no `version = \"...\"` in [{section}]")
    write(path, text[: m.start(2)] + new + text[m.end(2):])


# --- JSON: the top-level "version" (edited in place to keep formatting) -------
def json_version(path):
    data = json.loads(read(path))
    v = data.get("version")
    if not isinstance(v, str):
        die(f"{rel(path)}: no top-level string \"version\"")
    return v


def json_set_version(path, new):
    text = read(path)
    old = json_version(path)
    # The first `"version": "<old>"` at indent level 1 is the top-level key.
    pat = re.compile(r'(?m)^(  "version"\s*:\s*")' + re.escape(old) + r'(")')
    if len(pat.findall(text)) != 1:
        die(f"{rel(path)}: cannot locate the top-level \"version\" line uniquely")
    text = pat.sub(lambda m: m.group(1) + new + m.group(2), text, count=1)
    if json_version_from_text(text) != new:
        die(f"{rel(path)}: edit did not take effect")
    write(path, text)


def json_version_from_text(text):
    return json.loads(text).get("version")


# --- workspace members that inherit the workspace version ---------------------
def inheriting_packages():
    text = read(WS_TOML)
    m = re.search(r"(?ms)^members\s*=\s*\[(.*?)\]", text)
    if not m:
        die("Cargo.toml: no workspace members list")
    names = set()
    for member in re.findall(r'"([^"]+)"', m.group(1)):
        toml = os.path.join(root, member, "Cargo.toml")
        if not os.path.exists(toml):
            continue
        t = read(toml)
        if re.search(r"(?m)^version\s*(\.\s*workspace\s*=\s*true|=\s*\{\s*workspace\s*=\s*true)", t):
            n = re.search(r'(?m)^name\s*=\s*"([^"]+)"', t)
            if n:
                names.add(n.group(1))
    return names


# --- Cargo.lock: local (source-less) package entries --------------------------
LOCK_PKG = re.compile(r'(?ms)^\[\[package\]\]\nname = "([^"]+)"\nversion = "([^"]*)"\n(.*?)(?=^\[\[package\]\]|\Z)')


def lock_versions(path, names):
    """{name: version} for local packages in `names` (no `source =` line)."""
    out = {}
    if not os.path.exists(path):
        return out
    for m in LOCK_PKG.finditer(read(path)):
        name, ver, body = m.group(1), m.group(2), m.group(3)
        if name in names and not re.search(r"(?m)^source = ", body):
            out[name] = ver
    return out


def lock_set(path, names, new):
    if not os.path.exists(path):
        return
    text = read(path)

    def repl(m):
        name, ver, body = m.group(1), m.group(2), m.group(3)
        if name in names and not re.search(r"(?m)^source = ", body):
            return f'[[package]]\nname = "{name}"\nversion = "{new}"\n{body}'
        return m.group(0)

    write(path, LOCK_PKG.sub(repl, text))


# --- collect / check ----------------------------------------------------------
def current():
    ws_m, ws = toml_section_version(read(WS_TOML), "workspace.package")
    dk_m, dk = toml_section_version(read(DESK_TOML), "package")
    if ws is None:
        die("Cargo.toml: no version in [workspace.package]")
    if dk is None:
        die("desktop/src-tauri/Cargo.toml: no version in [package]")
    fields = {
        "Cargo.toml [workspace.package]": ws,
        "desktop/src-tauri/Cargo.toml [package]": dk,
        "desktop/src-tauri/tauri.conf.json": json_version(TAURI_CONF),
        "desktop/package.json": json_version(PKG_JSON),
    }
    inherit = inheriting_packages()
    for name, ver in sorted(lock_versions(WS_LOCK, inherit).items()):
        fields[f"Cargo.lock {name}"] = ver
    desk_names = inherit | {"opentune-desktop"}
    for name, ver in sorted(lock_versions(DESK_LOCK, desk_names).items()):
        fields[f"desktop/src-tauri/Cargo.lock {name}"] = ver
    return fields


def check(tag):
    fields = current()
    want = fields["desktop/src-tauri/tauri.conf.json"]
    if tag is not None:
        want = tag[1:] if tag.startswith("v") else tag
        if not SEMVER.match(want):
            die(f"tag '{tag}' is not v<semver> (expected e.g. v0.4.0 or v0.4.0-alpha.1)")
    bad = {k: v for k, v in fields.items() if v != want}
    if bad:
        src = f"tag {tag}" if tag is not None else "tauri.conf.json"
        print(f"bump-version: version mismatch (expected {want}, from {src}):", file=sys.stderr)
        locks = {}
        for k, v in bad.items():
            if "Cargo.lock " in k:
                lock = k.split(" ", 1)[0]
                locks.setdefault((lock, v), []).append(k.split(" ", 1)[1])
            else:
                print(f"  {k}: {v}", file=sys.stderr)
        for (lock, v), names in locks.items():
            print(f"  {lock}: {len(names)} local package(s) at {v} ({', '.join(names[:3])}"
                  + (", ...)" if len(names) > 3 else ")"), file=sys.stderr)
        print(f"fix: scripts/bump-version.sh {want}", file=sys.stderr)
        sys.exit(1)
    if not SEMVER.match(want):
        die(f"version '{want}' is not valid semver")
    print(f"version {want}: consistent across {len(fields)} fields"
          + (f", matches tag {tag}" if tag is not None else ""))


def set_version(new):
    if new.startswith("v"):
        new = new[1:]
    if "+" in new:
        die("build metadata (+...) is not allowed: GitHub rewrites '+' in asset names")
    if not SEMVER.match(new):
        die(f"'{new}' is not a valid version (MAJOR.MINOR.PATCH[-pre])", 2)
    inherit = inheriting_packages()
    toml_set_section_version(WS_TOML, "workspace.package", new)
    toml_set_section_version(DESK_TOML, "package", new)
    json_set_version(TAURI_CONF, new)
    json_set_version(PKG_JSON, new)
    lock_set(WS_LOCK, inherit, new)
    lock_set(DESK_LOCK, inherit | {"opentune-desktop"}, new)
    check(None)
    pre = "-" in new
    print(f"next: commit, merge to main, then  git tag v{new} && git push origin v{new}")
    if pre:
        print("note: a -pre version becomes a GitHub *pre-release*, which the in-app updater "
              "does NOT see (releases/latest skips pre-releases); see docs/RELEASING.md")


if args[0] == "--check":
    if len(args) > 2:
        die("usage: --check [TAG]", 2)
    check(args[1] if len(args) == 2 else None)
elif args[0] == "--print":
    print(current()["desktop/src-tauri/tauri.conf.json"])
elif args[0].startswith("-"):
    die(f"unknown option '{args[0]}' (try --help)", 2)
elif len(args) == 1:
    set_version(args[0])
else:
    die("usage: bump-version.sh X.Y.Z[-pre] | --check [TAG] | --print", 2)
PY
