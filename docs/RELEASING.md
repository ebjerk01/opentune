# Releasing OpenTune

Releases are cut by pushing a tag. [`.github/workflows/release.yml`](../.github/workflows/release.yml)
builds the `opentune` CLI for every platform and the Tauri desktop app, signs the
desktop update bundles, writes the updater manifest and creates **one** GitHub
release holding everything. The owner runbook (secrets, dry run, verifying an
update end to end) is in
[`desktop/UPDATER.md` → Publishing a signed release](../desktop/UPDATER.md#publishing-a-signed-release).

## Versions: one source of truth

```bash
scripts/bump-version.sh 0.4.0          # set every version field
scripts/bump-version.sh 0.4.0-alpha.1  # pre-release (see "Channels" below)
scripts/bump-version.sh --check        # all fields agree (CI, every push)
scripts/bump-version.sh --check v0.4.0 # ...and equal this tag (release CI)
```

It keeps these in step: `[workspace.package] version` in the root `Cargo.toml`
(inherited by every crate with `version.workspace = true`, including the CLI),
`desktop/src-tauri/Cargo.toml`, `desktop/src-tauri/tauri.conf.json` (the version
the updater compares), `desktop/package.json`, and the matching entries of both
`Cargo.lock` files. Crates that pin their own version, and the Android app, are
versioned separately. Build metadata (`+...`) is rejected because GitHub rewrites
`+` in asset names.

## Cutting a release

1. `scripts/bump-version.sh X.Y.Z`, move the `[Unreleased]` entries in
   [`CHANGELOG.md`](../CHANGELOG.md) into a `## [X.Y.Z] - YYYY-MM-DD` section
   (the release-scripts test fails if the current version has no section),
   commit, merge to `main`. The release notes include that section
   ([`scripts/changelog-section.py`](../scripts/changelog-section.py)).
2. `git tag -a vX.Y.Z -m "OpenTune X.Y.Z" && git push origin vX.Y.Z`.
3. The *Release* workflow runs:
   - **prepare** — tag = every version field; `tauri.conf.json`'s updater endpoint
     is this repository's `releases/latest/download/latest.json` and its pubkey
     is a valid minisign key; the `TAURI_SIGNING_PRIVATE_KEY` secret exists.
     Any failure stops the run before a build starts.
   - **cli** — `opentune-vX.Y.Z-<target>.tar.gz|zip` for Linux (ubuntu-22.04,
     glibc 2.35), Windows, macOS x86_64 and arm64.
   - **desktop** — Linux AppImage + `.deb` on ubuntu-22.04 (glibc baseline, see
     [`desktop/portable-build/`](../desktop/portable-build/README.md)), Windows NSIS
     `-setup.exe` (+ `.msi` for versions without a pre-release part, because WiX
     accepts only numeric pre-release identifiers). With
     `createUpdaterArtifacts: true` and the key, every bundle gets a `.sig`.
   - **publish** — [`scripts/make-latest-json.py`](../scripts/make-latest-json.py)
     verifies each `.sig` (Ed25519, key id must match the app's pubkey) and
     writes `latest.json`; `SHA256SUMS.txt` covers every asset; `gh release
     create` makes a **draft** (set the repository variable
     `OPENTUNE_RELEASE_AUTO_PUBLISH=true` to publish directly). A re-run replaces
     a draft's assets but never touches a published release.
4. Review the draft, then publish it ("Set as the latest release").

### Dry run

Actions → *Release* → **Run workflow** (`workflow_dispatch`) runs the same
builds with `--no-sign`, keeps the bundles as workflow artifacts and publishes
nothing. It needs no secrets.

### Desktop platforms

| Platform | Built | Updater key in `latest.json` |
| --- | --- | --- |
| Linux x86_64 AppImage | yes | `linux-x86_64`, `linux-x86_64-appimage` |
| Linux x86_64 `.deb` | yes | `linux-x86_64-deb` |
| Windows x86_64 NSIS | yes | `windows-x86_64`, `windows-x86_64-nsis` |
| Windows x86_64 MSI | full releases only | `windows-x86_64-msi` |
| macOS | **no** | — |

macOS desktop bundles are skipped on purpose: without an Apple Developer ID
(code signing + notarization) a downloaded `.app` is refused as "damaged" on
Apple silicon, and the project has no Mac to test on. The macOS **CLI** is still
built. Adding a signed macOS job later only needs the bundles in the publish
step; the generator already maps `.app.tar.gz` to `darwin-*`.

## Channels: full releases vs pre-releases

The app's updater endpoint is
`https://github.com/ebjerk01/opentune/releases/latest/download/latest.json`.
GitHub's `releases/latest` skips drafts **and pre-releases**, so:

- `vX.Y.Z` → full release → offered to every installed app once published.
- `vX.Y.Z-alpha.N` (any `-` part) → GitHub pre-release → **never offered** by the
  updater; install by hand. Such a build still updates to the next full release.

While the project is in alpha, ship the alpha series as plain `v0.x.y` tags; the
workflow titles every `0.x` release "OpenTune v0.x.y (early alpha)". An opt-in
beta channel would need a second endpoint and is not implemented.

## Testing the release helpers

```bash
scripts/test-release-scripts.sh   # bump-version + latest.json generator; CI job `release scripts`
```

It uses only a public minisign test vector — never the project's key.

## The signing key

The updater only installs bundles signed by the private key matching the
`pubkey` in `tauri.conf.json`. That key already exists and is held by the owner;
it reaches CI only as the `TAURI_SIGNING_PRIVATE_KEY` (+ `_PASSWORD`) secrets.
If it is ever lost or leaked, see the rotation note in
[`desktop/UPDATER.md`](../desktop/UPDATER.md): installed apps trust only the
baked-in public key, so a new key needs a transition release signed with the old
one.
