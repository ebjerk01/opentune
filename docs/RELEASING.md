# Releasing OpenTune

This document describes how to cut a tagged release. Releases are produced by
[`.github/workflows/release.yml`](../.github/workflows/release.yml), which builds
the `opentune` CLI for every supported platform and the Tauri desktop app
(including the auto-update artifacts), then attaches everything to the GitHub
Release for the tag.

## What a release produces

A push of a `vX.Y.Z` tag creates/updates the matching GitHub Release with:

**CLI archives** (one per target, binary named `opentune`):

- `opentune-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz`
- `opentune-vX.Y.Z-x86_64-pc-windows-msvc.zip`
- `opentune-vX.Y.Z-x86_64-apple-darwin.tar.gz`
- `opentune-vX.Y.Z-aarch64-apple-darwin.tar.gz`

**Desktop installers** (from `tauri-action`):

- Linux: `.AppImage` and `.deb`
- macOS: `.dmg` (and `.app.tar.gz`)
- Windows: `.msi` (and/or `.exe`)

**Desktop updater artifacts** (only when the signing secrets are configured):

- `latest.json` — the update manifest the desktop app polls
- A detached signature (`.sig`) next to each updater bundle

## One-time setup: the updater signing key

The desktop updater (Tauri's `tauri-plugin-updater`) will only install an update
whose artifacts are signed by a key that matches the public key baked into the
app. You generate this keypair **once** and reuse it for every release; if you
lose it, already-installed apps can no longer auto-update to new builds.

### 1. Generate the keypair

From the `desktop/` directory (so the local Tauri CLI is used):

```bash
cd desktop
npm install                      # once, to get @tauri-apps/cli
npx tauri signer generate -w ~/.tauri/opentune.key
```

`cargo tauri signer generate -w ~/.tauri/opentune.key` works too if you have the
CLI installed globally. You will be prompted for a password (keep it — it is the
second secret below). The command writes:

- `~/.tauri/opentune.key` — the **private** key (keep secret, never commit)
- `~/.tauri/opentune.key.pub` — the **public** key

### 2. Add the public key to `tauri.conf.json`

Copy the contents of the `.pub` file into
`desktop/src-tauri/tauri.conf.json` at `plugins.updater.pubkey`, replacing the
`REPLACE_WITH_TAURI_UPDATER_PUBLIC_KEY` placeholder:

```jsonc
"plugins": {
  "updater": {
    "endpoints": [
      "https://github.com/ebjerk01/opentune/releases/latest/download/latest.json"
    ],
    "pubkey": "<contents of opentune.key.pub>"
  }
}
```

Commit this change. The public key is not sensitive — it ships inside the app.

### 3. Store the private key + password as GitHub secrets

In the repository: **Settings → Secrets and variables → Actions → New
repository secret**. Add both:

| Secret name | Value |
| --- | --- |
| `TAURI_SIGNING_PRIVATE_KEY` | the full contents of `~/.tauri/opentune.key` |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | the password you chose when generating it |

The release workflow reads these as environment variables when building the
desktop bundles. **If they are absent, the workflow does not fail** — the desktop
job builds the installers with updater-artifact generation disabled (a dry run),
so you can exercise the pipeline before the key exists. Real auto-update releases
require both secrets.

## Cutting a release

1. Bump the version. Set the same `X.Y.Z` in:
   - the workspace `version` in the root `Cargo.toml` (`[workspace.package]`)
   - `desktop/src-tauri/tauri.conf.json` (`version`)
   - `desktop/package.json` (`version`)
2. Commit and merge to `main`.
3. Tag and push:

   ```bash
   git tag v1.2.3
   git push origin v1.2.3
   ```

4. The `Release` workflow runs automatically on the tag:
   - the **CLI** matrix builds and uploads the four archives, and
   - the **desktop** matrix builds the installers and (with the signing secrets
     set) signs and uploads the updater artifacts + `latest.json`.
5. Visit the Release on GitHub, confirm the assets are attached, edit the
   release notes if desired, and publish.

### Dry run (no publish)

Trigger the workflow manually from the **Actions** tab (**Run workflow** /
`workflow_dispatch`). It performs the same CLI and desktop builds but, because no
tag is present, uploads nothing — useful for validating the pipeline or a
platform build without creating a release.

## How desktop auto-update works

- The desktop app embeds the updater public key and the endpoint
  `https://github.com/ebjerk01/opentune/releases/latest/download/latest.json`
  (from `tauri.conf.json`).
- On each release, the workflow publishes a fresh `latest.json` (pointing at the
  new signed bundles) to the GitHub Release. The `latest/download/...` URL always
  resolves to the most recent non-draft, non-prerelease release.
- A running app periodically checks that endpoint. When `latest.json` advertises
  a version newer than the installed one, the updater downloads the platform
  bundle, verifies its signature against the embedded public key, and installs
  it. An update signed by any other key is rejected.

Because the public key is fixed in the app, keep the private key safe and reuse
it for every release. Rotating it requires shipping a new app build (via some
other channel) that carries the new public key before old installs can update
again.
