#!/usr/bin/env python3
"""Build the Tauri v2 updater manifest (latest.json) for a GitHub release.

The desktop app polls `plugins.updater.endpoints` in tauri.conf.json, which is

    https://github.com/<owner>/<repo>/releases/latest/download/latest.json

so latest.json must be a release asset whose platform URLs point at the other
assets of the SAME release:

    https://github.com/<owner>/<repo>/releases/download/<tag>/<asset>

Usage (release CI, after all desktop bundles + their .sig files are in DIR):

    scripts/make-latest-json.py --assets DIR --tag v0.4.0 --repo owner/repo \
        --out DIR/latest.json

    # only validate tauri.conf.json against the tag/repo (no assets needed):
    scripts/make-latest-json.py --check-config --tag v0.4.0 --repo owner/repo

What it checks, so a broken manifest never ships:
  * tauri.conf.json version == tag, and the updater endpoint points at this
    repo's releases/latest/download/latest.json;
  * the pubkey is a valid minisign Ed25519 public key;
  * every updater bundle (AppImage, deb, rpm, NSIS .exe, .msi, .app.tar.gz)
    carries the release version in its name and has a .sig next to it;
  * every .sig decodes to a minisign signature made by the pubkey's key id and,
    when the `cryptography` package is importable, verifies (Ed25519 over the
    BLAKE2b-512 prehash, plus the global signature over the trusted comment).
    `--require-verify` turns a missing `cryptography` into an error.
  * `--no-verify` skips all signature checks (unsigned dry runs with dummy .sig
    files only; never use it for a real release).

Platform keys follow tauri-plugin-updater 2.x, which looks up
`{os}-{arch}-{installer}` first and then `{os}-{arch}`:
  AppImage -> linux-<arch>-appimage + linux-<arch> (default for Linux)
  .deb     -> linux-<arch>-deb      .rpm -> linux-<arch>-rpm
  NSIS     -> windows-<arch>-nsis + windows-<arch> (default for Windows)
  .msi     -> windows-<arch>-msi (and windows-<arch> only if there is no NSIS)
  .app.tar.gz -> darwin-<arch>-app + darwin-<arch>
Standard library only (plus optional `cryptography`).
"""

import argparse
import base64
import binascii
import datetime
import hashlib
import json
import os
import re
import sys
from urllib.parse import quote

HERE = os.path.dirname(os.path.abspath(__file__))
DEFAULT_CONF = os.path.join(HERE, "..", "desktop", "src-tauri", "tauri.conf.json")

ARCH_TOKENS = {
    "amd64": "x86_64",
    "x86_64": "x86_64",
    "x64": "x86_64",
    "aarch64": "aarch64",
    "arm64": "aarch64",
    "i386": "i686",
    "i686": "i686",
    "x86": "i686",
    "armhf": "armv7",
    "armv7": "armv7",
}

# (suffix, os, installer) - the order decides which one becomes `{os}-{arch}`.
KINDS = [
    (".AppImage", "linux", "appimage"),
    (".deb", "linux", "deb"),
    (".rpm", "linux", "rpm"),
    ("-setup.exe", "windows", "nsis"),
    (".msi", "windows", "msi"),
    (".app.tar.gz", "darwin", "app"),
]
DEFAULT_INSTALLER = {"linux": ["appimage"], "windows": ["nsis", "msi"], "darwin": ["app"]}

# GitHub keeps these characters in asset names; anything else is rewritten,
# which would make the URL in latest.json point at nothing.
SAFE_ASSET = re.compile(r"^[A-Za-z0-9._-]+$")


class Fail(Exception):
    pass


def b64(s, what):
    try:
        return base64.b64decode(s.strip(), validate=True)
    except (binascii.Error, ValueError) as e:
        raise Fail(f"{what}: not valid base64 ({e})")


def parse_pubkey(pubkey):
    """Tauri pubkey (base64 of the minisign .pub text) or a bare `RW...` line.
    Returns (key_id_bytes, ed25519_public_key_32_bytes)."""
    raw = pubkey.strip()
    if not raw.startswith("RW"):
        text = b64(raw, "pubkey").decode("utf-8", "replace")
        lines = [l for l in text.splitlines() if l.strip()]
        if len(lines) != 2 or not lines[0].startswith("untrusted comment:"):
            raise Fail("pubkey: decoded text is not a two-line minisign public key")
        raw = lines[1].strip()
    blob = b64(raw, "pubkey key line")
    if len(blob) != 42 or blob[:2] != b"Ed":
        raise Fail(f"pubkey: expected 42 bytes starting 'Ed' (minisign Ed25519), got {len(blob)}")
    return blob[2:10], blob[10:]


def key_id_hex(kid):
    return kid[::-1].hex().upper()  # minisign prints the id little-endian


def parse_signature(sig_file_text, what):
    """A Tauri .sig file is base64 of the minisign signature text. Returns
    (latest_json_value, alg, key_id, sig64, trusted_comment, global_sig64)."""
    content = sig_file_text.strip()
    if content.startswith("untrusted comment:"):
        text = content
        content = base64.b64encode((text + "\n").encode()).decode()
    else:
        text = b64(content, what).decode("utf-8", "replace")
    lines = text.splitlines()
    if len(lines) < 4 or not lines[0].startswith("untrusted comment:") \
            or not lines[2].startswith("trusted comment: "):
        raise Fail(f"{what}: not a minisign signature (4 lines: untrusted comment, "
                   "signature, trusted comment, global signature)")
    blob = b64(lines[1], f"{what} signature line")
    if len(blob) != 74 or blob[:2] not in (b"Ed", b"ED"):
        raise Fail(f"{what}: signature line is not a minisign Ed25519 signature")
    glob = b64(lines[3], f"{what} global signature")
    if len(glob) != 64:
        raise Fail(f"{what}: global signature is not 64 bytes")
    trusted = lines[2][len("trusted comment: "):]
    return content, blob[:2], blob[2:10], blob[10:], trusted, glob


def ed25519_verifier():
    try:
        from cryptography.exceptions import InvalidSignature
        from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey
    except ImportError:
        return None

    def verify(pk, sig, msg):
        try:
            Ed25519PublicKey.from_public_bytes(pk).verify(sig, msg)
            return True
        except InvalidSignature:
            return False

    return verify


def verify_file(path, sig_text, pubkey, verifier, what):
    kid, pk = pubkey
    value, alg, skid, sig, trusted, glob = parse_signature(sig_text, what)
    if skid != kid:
        raise Fail(f"{what}: signed by key {key_id_hex(skid)}, but the app trusts "
                   f"{key_id_hex(kid)} - wrong TAURI_SIGNING_PRIVATE_KEY?")
    if verifier is not None:
        if alg == b"ED":
            h = hashlib.blake2b(digest_size=64)
            with open(path, "rb") as f:
                for chunk in iter(lambda: f.read(1 << 20), b""):
                    h.update(chunk)
            msg = h.digest()
        else:
            with open(path, "rb") as f:
                msg = f.read()
        if not verifier(pk, sig, msg):
            raise Fail(f"{what}: signature does NOT verify against the file")
        if not verifier(pk, glob, sig + trusted.encode()):
            raise Fail(f"{what}: global signature over the trusted comment does NOT verify")
    return value


def arch_of(name):
    for tok in re.split(r"[_.-]", name):
        if tok in ARCH_TOKENS:
            return ARCH_TOKENS[tok]
    return None


def load_conf(path):
    with open(path, encoding="utf-8") as f:
        conf = json.load(f)
    upd = conf.get("plugins", {}).get("updater", {})
    return conf.get("version"), upd.get("pubkey", ""), upd.get("endpoints", [])


def check_config(conf_path, version, server, repo, notes):
    conf_version, pubkey, endpoints = load_conf(conf_path)
    if conf_version != version:
        raise Fail(f"tauri.conf.json version is {conf_version!r}, release is {version!r} "
                   "(run scripts/bump-version.sh)")
    key = parse_pubkey(pubkey)
    want = f"{server}/{repo}/releases/latest/download/latest.json"
    if want not in endpoints:
        raise Fail(f"updater endpoints {endpoints} do not include {want}; installed apps "
                   "would never see this release's latest.json")
    notes.append(f"config ok: version {version}, pubkey id {key_id_hex(key[0])}, endpoint {want}")
    return key


def build(args):
    version = args.tag[1:] if args.tag.startswith("v") else args.tag
    log = []
    pubkey = check_config(args.conf, version, args.server_url.rstrip("/"), args.repo, log)
    if args.check_config:
        return None, log

    verifier = None
    if not args.no_verify:
        verifier = ed25519_verifier()
        if verifier is None:
            if args.require_verify:
                raise Fail("--require-verify: python `cryptography` is not importable")
            log.append("warning: `cryptography` missing - key ids checked, signatures not verified")

    base = f"{args.server_url.rstrip('/')}/{args.repo}/releases/download/{quote(args.tag)}"
    found = {}  # (os, arch) -> {installer: entry}
    for name in sorted(os.listdir(args.assets)):
        kind = next((k for k in KINDS if name.endswith(k[0])), None)
        if kind is None:
            continue
        _, osname, installer = kind
        path = os.path.join(args.assets, name)
        if not SAFE_ASSET.match(name):
            raise Fail(f"{name}: GitHub would rename this asset (allowed: A-Z a-z 0-9 . _ -)")
        if f"_{version}_" not in name and not name.endswith(f"_{version}{kind[0]}"):
            raise Fail(f"{name}: does not carry version {version} - stale bundle?")
        arch = arch_of(name[: -len(kind[0])])
        if arch is None:
            raise Fail(f"{name}: cannot tell the CPU architecture from the file name")
        sig_path = path + ".sig"
        if not os.path.exists(sig_path):
            raise Fail(f"{name}: no {name}.sig - was TAURI_SIGNING_PRIVATE_KEY set for the build?")
        with open(sig_path, encoding="utf-8") as f:
            sig_text = f.read()
        if args.no_verify:
            signature = sig_text.strip()
        else:
            signature = verify_file(path, sig_text, pubkey, verifier, f"{name}.sig")
        slot = found.setdefault((osname, arch), {})
        if installer in slot:
            raise Fail(f"two {installer} bundles for {osname}-{arch}: "
                       f"{slot[installer]['url']} and {name}")
        slot[installer] = {"signature": signature, "url": f"{base}/{quote(name)}"}
        log.append(f"{osname}-{arch}-{installer}: {name}" +
                   (" (signature NOT checked)" if args.no_verify else " (signature ok)"))

    if not found:
        raise Fail(f"no updater bundles (.AppImage/.deb/.rpm/-setup.exe/.msi/.app.tar.gz) in {args.assets}")

    platforms = {}
    for (osname, arch), slot in sorted(found.items()):
        for installer, entry in sorted(slot.items()):
            platforms[f"{osname}-{arch}-{installer}"] = entry
        default = next((i for i in DEFAULT_INSTALLER[osname] if i in slot), None)
        if default:
            platforms[f"{osname}-{arch}"] = slot[default]

    pub_date = args.pub_date or datetime.datetime.now(datetime.timezone.utc) \
        .replace(microsecond=0).isoformat().replace("+00:00", "Z")
    notes = args.notes if args.notes is not None else (
        f"OpenTune {args.tag}. Release notes: "
        f"{args.server_url.rstrip('/')}/{args.repo}/releases/tag/{quote(args.tag)}")
    manifest = {"version": version, "notes": notes, "pub_date": pub_date, "platforms": platforms}
    return manifest, log


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--tag", required=True, help="release tag, e.g. v0.4.0")
    ap.add_argument("--repo", required=True, help="owner/name (GITHUB_REPOSITORY)")
    ap.add_argument("--server-url", default="https://github.com", help="GITHUB_SERVER_URL")
    ap.add_argument("--conf", default=DEFAULT_CONF, help="tauri.conf.json")
    ap.add_argument("--assets", help="directory holding the bundles and their .sig files")
    ap.add_argument("--out", help="where to write latest.json (default: stdout)")
    ap.add_argument("--notes", help="update notes shown in the app (default: link to the release)")
    ap.add_argument("--pub-date", help="RFC 3339 date (default: now, UTC)")
    ap.add_argument("--check-config", action="store_true", help="only validate tauri.conf.json")
    ap.add_argument("--require-verify", action="store_true",
                    help="fail if signatures cannot be cryptographically verified")
    ap.add_argument("--no-verify", action="store_true",
                    help="DRY RUN ONLY: take .sig files as-is (dummy signatures)")
    args = ap.parse_args()
    if not args.check_config and not args.assets:
        ap.error("--assets is required (or use --check-config)")
    if args.no_verify and args.require_verify:
        ap.error("--no-verify and --require-verify are mutually exclusive")

    try:
        manifest, log = build(args)
    except (Fail, OSError, json.JSONDecodeError) as e:
        print(f"make-latest-json: ERROR: {e}", file=sys.stderr)
        sys.exit(1)
    for line in log:
        print(f"make-latest-json: {line}", file=sys.stderr)
    if manifest is None:
        return
    text = json.dumps(manifest, indent=2) + "\n"
    if args.out:
        with open(args.out, "w", encoding="utf-8") as f:
            f.write(text)
        print(f"make-latest-json: wrote {args.out} ({len(manifest['platforms'])} platform keys)",
              file=sys.stderr)
    else:
        sys.stdout.write(text)


if __name__ == "__main__":
    main()
